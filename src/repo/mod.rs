//! IPFS repo
use crate::block::{Cid, Block};
use crate::error::Error;
use crate::future::BlockFuture;
use crate::path::IpfsPath;
use crate::IpfsOptions;
use core::future::Future;
use futures::future::FutureObj;
use futures::join;
use libp2p::PeerId;
use std::marker::PhantomData;
use std::path::PathBuf;
use std::sync::mpsc::{channel, Sender, Receiver};

pub mod mem;
pub mod fs;

pub trait RepoTypes: Clone + Send + Sync + 'static {
    type TBlockStore: BlockStore;
    type TDataStore: DataStore;
}

#[derive(Clone, Debug)]
pub struct RepoOptions<TRepoTypes: RepoTypes> {
    _marker: PhantomData<TRepoTypes>,
    path: PathBuf,
}

impl<TRepoTypes: RepoTypes> From<&IpfsOptions<TRepoTypes>> for RepoOptions<TRepoTypes> {
    fn from(options: &IpfsOptions<TRepoTypes>) -> Self {
        RepoOptions {
            _marker: PhantomData,
            path: options.ipfs_path.clone(),
        }
    }
}

pub fn create_repo<TRepoTypes: RepoTypes>(options: RepoOptions<TRepoTypes>) -> (Repo<TRepoTypes>, Receiver<RepoEvent>) {
    Repo::new(options)
}

pub trait BlockStore: Clone + Send + Sync + Unpin + 'static {
    fn new(path: PathBuf) -> Self;
    fn init(&self) ->
        FutureObj<'static, Result<(), Error>>;
    fn open(&self) ->
        FutureObj<'static, Result<(), Error>>;
    fn contains(&self, cid: &Cid) ->
        FutureObj<'static, Result<bool, Error>>;
    fn get(&self, cid: &Cid) ->
        FutureObj<'static, Result<Option<Block>, Error>>;
    fn put(&self, block: Block) ->
        FutureObj<'static, Result<Cid, Error>>;
    fn remove(&self, cid: &Cid) ->
        FutureObj<'static, Result<(), Error>>;
}

pub trait DataStore: Clone + Send + Sync + Unpin + 'static {
    fn new(path: PathBuf) -> Self;
    fn init(&self) ->
        FutureObj<'static, Result<(), Error>>;
    fn open(&self) ->
        FutureObj<'static, Result<(), Error>>;
    fn contains(&self, col: Column, key: &[u8]) ->
        FutureObj<'static, Result<bool, Error>>;
    fn get(&self, col: Column, key: &[u8]) ->
        FutureObj<'static, Result<Option<Vec<u8>>, Error>>;
    fn put(&self, col: Column, key: &[u8], value: &[u8]) ->
        FutureObj<'static, Result<(), Error>>;
    fn remove(&self, col: Column, key: &[u8]) ->
        FutureObj<'static, Result<(), Error>>;
}

#[derive(Clone, Copy, Debug)]
pub enum Column {
    Ipns
}

#[derive(Clone, Debug)]
pub struct Repo<TRepoTypes: RepoTypes> {
    block_store: TRepoTypes::TBlockStore,
    data_store: TRepoTypes::TDataStore,
    events: Sender<RepoEvent>,
}

#[derive(Clone, Debug)]
pub enum RepoEvent {
    WantBlock(Cid),
    ProvideBlock(Cid),
    UnprovideBlock(Cid),
}

impl<TRepoTypes: RepoTypes> Repo<TRepoTypes> {
    pub fn new(options: RepoOptions<TRepoTypes>) -> (Self, Receiver<RepoEvent>) {
        let mut blockstore_path = options.path.clone();
        let mut datastore_path = options.path;
        blockstore_path.push("blockstore");
        datastore_path.push("datastore");
        let block_store = TRepoTypes::TBlockStore::new(blockstore_path);
        let data_store = TRepoTypes::TDataStore::new(datastore_path);
        let (sender, receiver) = channel::<RepoEvent>();
        (Repo {
            block_store,
            data_store,
            events: sender,
        }, receiver)
    }

    pub fn init(&self) -> impl Future<Output=Result<(), Error>> {
        let block_store = self.block_store.clone();
        let data_store = self.data_store.clone();
        async move {
            let f1 = block_store.init();
            let f2 = data_store.init();
            let (r1, r2) = join!(f1, f2);
            if r1.is_err() {
                r1
            } else {
                r2
            }
        }
    }

    pub fn open(&self) -> impl Future<Output=Result<(), Error>> {
        let block_store = self.block_store.clone();
        let data_store = self.data_store.clone();
        async move {
            let f1 = block_store.open();
            let f2 = data_store.open();
            let (r1, r2) = join!(f1, f2);
            if r1.is_err() {
                r1
            } else {
                r2
            }
        }
    }

    /// Puts a block into the block store.
    pub fn put_block(&self, block: Block) ->
    impl Future<Output=Result<Cid, Error>>
    {
        let events = self.events.clone();
        let block_store = self.block_store.clone();
        async move {
            let cid = await!(block_store.put(block))?;
            // sending only fails if no one is listening anymore
            // and that is okay with us.
            let _ = events.send(RepoEvent::ProvideBlock(cid.clone()));
            Ok(cid)
        }
    }

    /// Retrives a block from the block store.
    pub fn get_block(&self, cid: &Cid) ->
    impl Future<Output=Result<Block, Error>>
    {
        let cid = cid.to_owned();
        let events = self.events.clone();
        let block_store = self.block_store.clone();
        async move {
            if !await!(block_store.contains(&cid))? {
                // sending only fails if no one is listening anymore
                // and that is okay with us.
                let _ = events.send(RepoEvent::WantBlock(cid.clone()));
            }
            await!(BlockFuture::new(block_store, cid))
        }
    }

    /// Remove block from the block store.
    pub fn remove_block(&self, cid: &Cid)
        -> impl Future<Output=Result<(), Error>>
    {
        // sending only fails if no one is listening anymore
        // and that is okay with us.
        let _ = self.events.send(RepoEvent::UnprovideBlock(cid.to_owned()));
        self.block_store.remove(cid)
    }

    /// Get an ipld path from the datastore.
    pub fn get_ipns(&self, ipns: &PeerId) ->
    impl Future<Output=Result<Option<IpfsPath>, Error>>
    {
        let data_store = self.data_store.clone();
        let key = ipns.to_owned();
        async move {
            let bytes = await!(data_store.get(Column::Ipns, key.as_bytes()))?;
            match bytes {
                Some(ref bytes) => {
                    let string = String::from_utf8_lossy(bytes);
                    let path = IpfsPath::from_str(&string)?;
                    Ok(Some(path))
                }
                None => Ok(None)
            }
        }
    }

    /// Put an ipld path into the datastore.
    pub fn put_ipns(&self, ipns: &PeerId, path: &IpfsPath) ->
    impl Future<Output=Result<(), Error>>
    {
        let string = path.to_string();
        let value = string.as_bytes();
        self.data_store.put(Column::Ipns, ipns.as_bytes(), value)
    }

    /// Remove an ipld path from the datastore.
    pub fn remove_ipns(&self, ipns: &PeerId) ->
    impl Future<Output=Result<(), Error>>
    {
        self.data_store.remove(Column::Ipns, ipns.as_bytes())
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use std::env::temp_dir;

    #[derive(Clone)]
    pub struct Types;

    impl RepoTypes for Types {
        type TBlockStore = mem::MemBlockStore;
        type TDataStore = mem::MemDataStore;
    }

    pub fn create_mock_repo() -> Repo<Types> {
        let mut tmp = temp_dir();
        tmp.push("rust-ipfs-repo");
        let options: RepoOptions<Types> = RepoOptions {
            _marker: PhantomData,
            path: tmp,
        };
        let (r, _) = Repo::new(options);
        r
    }

    #[test]
    fn test_repo() {
        let mut tmp = temp_dir();
        tmp.push("rust-ipfs-repo");
        let options: RepoOptions<Types> = RepoOptions {
            _marker: PhantomData,
            path: tmp,
        };
        let (repo, _) = Repo::new(options);
        tokio::run_async(async move {
            await!(repo.init()).unwrap();
        });
    }
}
