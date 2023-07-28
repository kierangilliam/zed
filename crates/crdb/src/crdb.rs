mod btree;
mod dense_id;
mod messages;
mod operations;
mod rope;
mod sync;
#[cfg(test)]
mod test;

use anyhow::{anyhow, Result};
use btree::{Bias, KvStore};
use collections::{btree_map, BTreeMap, BTreeSet, Bound, HashMap, HashSet, VecDeque};
use dense_id::DenseId;
use futures::{channel::mpsc, future::BoxFuture, FutureExt, StreamExt};
use messages::{MessageEnvelope, Operation, RequestEnvelope};
use parking_lot::{Mutex, RwLock};
use rope::Rope;
use serde::{Deserialize, Serialize};
use smallvec::{smallvec, SmallVec};
use std::{
    any::{Any, TypeId},
    cmp::{self, Ordering},
    fmt::{self, Debug, Display},
    future::Future,
    ops::Range,
    path::Path,
    sync::{Arc, Weak},
};
use util::ResultExt;
use uuid::Uuid;

const CHUNK_SIZE: usize = 64;

#[derive(Copy, Clone, Default, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct RepoId(Uuid);

impl RepoId {
    fn new() -> Self {
        #[cfg(not(any(test, feature = "test-support")))]
        return RepoId(Uuid::new_v4());

        #[cfg(any(test, feature = "test-support"))]
        {
            use std::sync::atomic::{AtomicUsize, Ordering::SeqCst};
            static NEXT_REPO_ID: AtomicUsize = AtomicUsize::new(0);
            RepoId(Uuid::from_u128(NEXT_REPO_ID.fetch_add(1, SeqCst) as u128))
        }
    }
}

impl Display for RepoId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        #[cfg(not(any(test, feature = "test-support")))]
        return write!(f, "{}", self.0.as_hyphenated());

        #[cfg(any(test, feature = "test-support"))]
        return write!(f, "{}", self.0.as_u128());
    }
}

impl Debug for RepoId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        #[cfg(not(any(test, feature = "test-support")))]
        return write!(f, "RepoId({})", self.0.as_hyphenated());

        #[cfg(any(test, feature = "test-support"))]
        return write!(f, "RepoId({})", self.0.as_u128());
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize, Hash)]
pub struct RevisionId(SmallVec<[OperationId; 2]>);

impl From<OperationId> for RevisionId {
    fn from(id: OperationId) -> Self {
        RevisionId(smallvec![id])
    }
}

impl RevisionId {
    fn len(&self) -> usize {
        self.0.len()
    }

    fn observe(&mut self, operation_id: OperationId, parent: &Self) {
        if parent.0.iter().all(|op_id| self.0.contains(op_id)) {
            self.0.retain(|op_id| !parent.0.contains(op_id));
        }
        self.0.push(operation_id);
        self.0.sort();
    }

    fn iter(&self) -> impl Iterator<Item = &OperationId> {
        self.0.iter()
    }
}

#[derive(
    Copy, Clone, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize, Hash,
)]
pub struct ReplicaId(u32);

#[derive(
    Copy, Clone, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize, Hash,
)]
pub struct OperationCount(usize);

#[derive(
    Copy, Clone, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize, Hash,
)]
pub struct OperationId {
    pub replica_id: ReplicaId,
    pub operation_count: OperationCount,
}

impl OperationId {
    pub fn new(replica_id: ReplicaId) -> Self {
        Self {
            replica_id,
            operation_count: OperationCount::default(),
        }
    }

    pub fn tick(&mut self) -> OperationId {
        self.operation_count.0 += 1;
        *self
    }

    pub fn observe(&mut self, other: Self) {
        self.operation_count.0 = cmp::max(self.operation_count.0, other.operation_count.0) + 1;
    }

    pub fn is_causally_after(&self, id: Self) -> bool {
        self.operation_count > id.operation_count
            || (self.operation_count == id.operation_count && self.replica_id > id.replica_id)
    }
}

impl btree::Summary for OperationId {
    type Context = ();

    fn add_summary(&mut self, summary: &Self, _: &()) {
        debug_assert!(summary > self);
        *self = *summary;
    }
}

#[derive(Clone, Debug, Ord, PartialOrd, Eq, PartialEq, Serialize, Deserialize)]
pub struct RoomName(Arc<str>);

#[derive(Clone, Debug, Ord, PartialOrd, Eq, PartialEq, Serialize, Deserialize)]
pub struct RoomToken(Arc<str>);

#[derive(Clone)]
pub struct User {
    login: Arc<str>,
}

pub trait Request: Message + Into<RequestEnvelope> {
    type Response: Message;
}

pub trait Message: 'static + Send {
    fn from_bytes(bytes: Vec<u8>) -> Result<Self>
    where
        Self: Sized;
    fn to_bytes(&self) -> Vec<u8>;
}

impl<T> Message for T
where
    T: 'static + Send + Serialize + for<'a> Deserialize<'a>,
{
    fn from_bytes(bytes: Vec<u8>) -> Result<Self> {
        Ok(serde_bare::from_slice(&bytes)?)
    }

    fn to_bytes(&self) -> Vec<u8> {
        serde_bare::to_vec(self).unwrap()
    }
}

pub trait ServerNetwork: 'static + Send + Sync {
    fn handle_requests<H, F>(&self, handle_request: H)
    where
        H: 'static + Send + Fn(User, Vec<u8>) -> Result<F>,
        F: 'static + Send + futures::Future<Output = Result<Vec<u8>>>;
    fn create_room(&self, room: &RoomName) -> BoxFuture<Result<()>>;
    fn grant_room_access(&self, room: &RoomName, user: &str) -> RoomToken;
}

pub trait ClientNetwork: 'static + Send + Sync {
    type Room: ClientRoom;

    fn request(&self, request: Vec<u8>) -> BoxFuture<Result<Vec<u8>>>;
    fn room(&self, credentials: RoomCredentials) -> Self::Room;
}

pub trait ClientRoom: 'static + Send + Sync {
    fn connect(&mut self) -> BoxFuture<Result<()>>;
    fn broadcast(&self, message: Vec<u8>);
    fn handle_messages(&self, handle_message: impl 'static + Send + Fn(Vec<u8>));
}

pub trait Executor: 'static + Send + Sync {
    fn spawn<F>(&self, future: F)
    where
        F: 'static + Send + Future<Output = ()>;
}

struct Client<E, N: ClientNetwork> {
    db: Db,
    network: Arc<N>,
    checkouts: Mutex<HashMap<RepoId, Checkout<E, N>>>,
    executor: Arc<E>,
    repo_snapshots: mpsc::UnboundedSender<(RepoId, RepoSnapshot)>,
}

struct Checkout<E, N: ClientNetwork> {
    repo: Repo,
    network_room: Arc<N::Room>,
    operations_tx: mpsc::UnboundedSender<Operation>,
    message_handlers: Arc<
        RwLock<HashMap<TypeId, Box<dyn Send + Sync + Fn(&Client<E, N>, RepoId, Box<dyn Any>)>>>,
    >,
}

impl<E, N: ClientNetwork> Clone for Checkout<E, N> {
    fn clone(&self) -> Self {
        Self {
            repo: self.repo.clone(),
            network_room: self.network_room.clone(),
            operations_tx: self.operations_tx.clone(),
            message_handlers: self.message_handlers.clone(),
        }
    }
}

impl<E: Executor, N: ClientNetwork> Checkout<E, N> {
    fn new(client: &Arc<Client<E, N>>, repo: Repo, network_room: N::Room) -> Self {
        let (operations_tx, operations_rx) = mpsc::unbounded();
        let this = Self {
            repo: repo.clone(),
            network_room: Arc::new(network_room),
            operations_tx,
            message_handlers: Default::default(),
        };

        {
            let handlers = this.message_handlers.clone();
            let client = Arc::downgrade(&client);
            this.network_room.handle_messages(move |message| {
                if let Some(envelope) =
                    serde_bare::from_slice::<MessageEnvelope>(&message).log_err()
                {
                    let message = envelope.unwrap();
                    if let Some(client) = client.upgrade() {
                        if let Some(handler) = handlers.read().get(&message.as_ref().type_id()) {
                            handler(&client, repo.id, message);
                        }
                    }
                };
            });
        }

        client.executor.spawn({
            let this = this.clone();
            let client = Arc::downgrade(client);
            async move {
                if let Some(client) = client.upgrade() {
                    this.sync(&client).await.expect("network is infallible");
                }

                let mut operations_rx = operations_rx.ready_chunks(CHUNK_SIZE);
                while let Some(operations) = operations_rx.next().await {
                    if let Some(client) = client.upgrade() {
                        client
                            .request(messages::PublishOperations {
                                repo_id: this.repo.id,
                                operations: operations.clone(),
                            })
                            .await
                            .expect("network is infallible");

                        for operation in operations {
                            this.network_room
                                .broadcast(MessageEnvelope::Operation(operation).to_bytes());
                        }
                    } else {
                        break;
                    }
                }
            }
        });

        this
    }

    fn handle_messages<M: Message, H>(&self, handle_message: H)
    where
        M: Message,
        H: 'static + Fn(&Client<E, N>, RepoId, M) + Send + Sync,
    {
        self.message_handlers.write().insert(
            TypeId::of::<M>(),
            Box::new(move |client, repo_id, message| {
                handle_message(client, repo_id, *message.downcast().unwrap())
            }),
        );
    }

    fn broadcast_operation(&self, operation: Operation) {
        self.operations_tx.unbounded_send(operation).unwrap();
    }

    async fn sync(&self, client: &Client<E, N>) -> Result<()> {
        let response = client
            .request(messages::SyncRepo {
                id: self.repo.id,
                max_operation_ids: self.repo.read(|repo| (&repo.max_operation_ids).into()),
            })
            .await?;
        self.repo.update(|repo| {
            repo.apply_operations(response.operations);
            (None, ())
        });

        let operations = self
            .repo
            .read(|snapshot| snapshot.operations_since(&(&response.max_operation_ids).into()));

        for chunk in operations.chunks(CHUNK_SIZE) {
            client
                .request(messages::PublishOperations {
                    repo_id: self.repo.id,
                    operations: chunk.to_vec(),
                })
                .await?;
            for operation in chunk {
                self.network_room
                    .broadcast(MessageEnvelope::Operation(operation.clone()).to_bytes());
            }
        }

        Ok(())
    }
}

#[derive(Clone, Serialize, Deserialize)]
pub struct RoomCredentials {
    name: RoomName,
    token: RoomToken,
}

impl<E: Executor, N: ClientNetwork> Client<E, N> {
    pub fn new(executor: E, network: N, kv: Arc<dyn KvStore>) -> Arc<Self> {
        Arc::new_cyclic(|weak_self: &Weak<Self>| {
            let (repo_snapshots_tx, mut repo_snapshots_rx) = mpsc::unbounded();
            let mut this = Self {
                db: Db::new(kv),
                network: Arc::new(network),
                checkouts: Default::default(),
                executor: Arc::new(executor),
                repo_snapshots: repo_snapshots_tx,
            };
            this.db.on_local_operation({
                let this = weak_self.clone();
                move |repo_id, operation| {
                    if let Some(this) = this.upgrade() {
                        this.handle_local_operation(repo_id, operation)
                    }
                }
            });
            this.db.on_repo_snapshot_changed({
                let this = weak_self.clone();
                move |repo_id, snapshot| {
                    if let Some(this) = this.upgrade() {
                        let _ = this
                            .repo_snapshots
                            .unbounded_send((repo_id, snapshot.clone()));
                    }
                }
            });
            this.executor.spawn({
                let this = weak_self.clone();
                async move {
                    while let Some((repo_id, snapshot)) = repo_snapshots_rx.next().await {
                        if let Some(this) = this.upgrade() {
                            snapshot.save(repo_id, &*this.db.kv).await.log_err();
                        } else {
                            break;
                        }
                    }
                }
            });

            this
        })
    }

    pub fn create_repo(&self) -> Repo {
        let id = RepoId::new();
        let snapshot = RepoSnapshot::default();
        let repo = Repo {
            id,
            db: self.db.clone(),
        };
        self.db.snapshot.lock().repos.insert(id, snapshot);
        repo
    }

    pub fn clone_repo(
        self: &Arc<Self>,
        name: impl Into<Arc<str>>,
    ) -> impl Future<Output = Result<Repo>> {
        let this = self.clone();
        let name = name.into();
        async move {
            let response = this.request(messages::CloneRepo { name }).await?;
            let repo_id = response.repo_id;
            let repo = Repo {
                id: repo_id,
                db: this.db.clone(),
            };
            this.db
                .snapshot
                .lock()
                .repos
                .insert(repo_id, Default::default());

            let mut room = this.network.room(response.credentials);
            room.connect().await?;
            let checkout = Checkout::new(&this, repo.clone(), room);
            checkout.handle_messages(Self::handle_remote_operation);
            this.checkouts.lock().insert(repo_id, checkout);

            Ok(repo)
        }
    }

    pub fn publish_repo(
        self: &Arc<Self>,
        repo: &Repo,
        name: impl Into<Arc<str>>,
    ) -> impl Future<Output = Result<()>> {
        let this = self.clone();
        let name = name.into();
        let repo = repo.clone();
        async move {
            let response = this
                .request(messages::PublishRepo { id: repo.id, name })
                .await?;
            let mut room = this.network.room(response.credentials);
            room.connect().await?;
            let checkout = Checkout::new(&this, repo.clone(), room);
            checkout.handle_messages(Self::handle_remote_operation);
            this.checkouts.lock().insert(repo.id, checkout);

            Ok(())
        }
    }

    pub fn is_local_repo(&self, repo: &Repo) -> bool {
        !self.checkouts.lock().contains_key(&repo.id)
    }

    pub fn repo(self: &Arc<Self>, id: RepoId) -> impl Future<Output = Result<Repo>> {
        let this = self.clone();
        async move {
            if let Some(repo) = this.db.repo(id) {
                return Ok(repo);
            }

            let repo = RepoSnapshot::load(id, &*this.db.kv).await?;
            let mut db = this.db.snapshot.lock();
            if !db.repos.contains_key(&id) {
                db.repos.insert(id, repo);
            }

            Ok(Repo {
                id,
                db: this.db.clone(),
            })
        }
    }

    pub fn repos(&self) -> Vec<Repo> {
        self.db
            .snapshot
            .lock()
            .repos
            .iter()
            .map(|(id, _)| Repo {
                id: *id,
                db: self.db.clone(),
            })
            .collect()
    }

    fn handle_local_operation(&self, repo_id: RepoId, operation: Operation) {
        if let Some(checkout) = self.checkouts.lock().get(&repo_id) {
            checkout.broadcast_operation(operation);
        }
    }

    fn handle_remote_operation(&self, repo_id: RepoId, operation: Operation) {
        let repo = self.db.repo(repo_id).expect("repo must exist");
        repo.update(|repo| {
            repo.apply_operations([operation]);
            (None, ())
        });
    }

    fn request<R: Request>(&self, request: R) -> BoxFuture<Result<R::Response>> {
        let envelope: RequestEnvelope = request.into();
        let response = self.network.request(envelope.to_bytes());
        async { Ok(R::Response::from_bytes(response.await?)?) }.boxed()
    }
}

struct Server<N> {
    db: Db,
    network: Arc<N>,
    request_handlers: Arc<
        RwLock<
            BTreeMap<
                TypeId,
                Box<
                    dyn Send + Sync + Fn(User, Box<dyn Any>) -> BoxFuture<'static, Result<Vec<u8>>>,
                >,
            >,
        >,
    >,
    repo_ids_by_name: Arc<Mutex<BTreeMap<Arc<str>, RepoId>>>,
    next_replica_ids_by_repo_id: Arc<Mutex<BTreeMap<RepoId, ReplicaId>>>,
}

impl<N: ServerNetwork> Clone for Server<N> {
    fn clone(&self) -> Self {
        Self {
            db: self.db.clone(),
            network: self.network.clone(),
            repo_ids_by_name: self.repo_ids_by_name.clone(),
            request_handlers: self.request_handlers.clone(),
            next_replica_ids_by_repo_id: self.next_replica_ids_by_repo_id.clone(),
        }
    }
}

impl<N: ServerNetwork> Server<N> {
    fn new(network: N, kv: Arc<dyn KvStore>) -> Self {
        let network = Arc::new(network);
        let this = Self {
            db: Db::new(kv),
            network: network.clone(),
            request_handlers: Default::default(),
            repo_ids_by_name: Default::default(),
            next_replica_ids_by_repo_id: Default::default(),
        };

        this.handle_requests(Self::handle_publish_repo);
        this.handle_requests(Self::handle_clone_repo);
        this.handle_requests(Self::handle_sync_repo);
        this.handle_requests(Self::handle_publish_operations);
        let request_handlers = this.request_handlers.clone();

        network.handle_requests(move |user, request_bytes| {
            let envelope = RequestEnvelope::from_bytes(request_bytes)?;
            let request = envelope.unwrap();
            let request_handlers = request_handlers.read();
            let request_handler = request_handlers
                .get(&request.as_ref().type_id())
                .ok_or_else(|| anyhow!("no request handler"))?;
            let response = (request_handler)(user, request);
            Ok(response)
        });

        this
    }

    fn handle_requests<F, Fut, R>(&self, handle_request: F)
    where
        F: 'static + Send + Sync + Fn(Self, User, R) -> Fut,
        Fut: 'static + Send + Future<Output = Result<R::Response>>,
        R: Request,
    {
        let request_handlers = self.request_handlers.clone();

        request_handlers.write().insert(
            TypeId::of::<R>(),
            Box::new({
                let this = self.clone();
                move |user, request| {
                    let request = *request.downcast::<R>().unwrap();
                    let response = handle_request(this.clone(), user, request);
                    async move {
                        let response = response.await;
                        response.map(|response| response.to_bytes())
                    }
                    .boxed()
                }
            }),
        );
    }

    async fn handle_publish_repo(
        self,
        user: User,
        request: messages::PublishRepo,
    ) -> Result<messages::PublishRepoResponse> {
        let room_name = RoomName(request.id.to_string().into());
        self.network.create_room(&room_name).await?;

        // TODO: handle repositories that had already been published.
        match self.repo_ids_by_name.lock().entry(request.name.clone()) {
            btree_map::Entry::Occupied(_) => return Err(anyhow!("repo name taken")),
            btree_map::Entry::Vacant(entry) => {
                entry.insert(request.id);
            }
        }

        let token = self
            .network
            .grant_room_access(&room_name, user.login.as_ref());

        self.db
            .snapshot
            .lock()
            .repos
            .insert(request.id, Default::default());
        self.next_replica_ids_by_repo_id
            .lock()
            .insert(request.id, ReplicaId(1));

        Ok(messages::PublishRepoResponse {
            credentials: RoomCredentials {
                name: room_name,
                token,
            },
        })
    }

    async fn handle_clone_repo(
        self,
        user: User,
        request: messages::CloneRepo,
    ) -> Result<messages::CloneRepoResponse> {
        let repo_id = *self
            .repo_ids_by_name
            .lock()
            .get(&request.name)
            .ok_or_else(|| anyhow!("repo not found"))?;
        let name = RoomName(repo_id.to_string().into());
        let token = self.network.grant_room_access(&name, user.login.as_ref());
        let replica_id = {
            let mut next_replica_ids = self.next_replica_ids_by_repo_id.lock();
            let next_replica_id = next_replica_ids.get_mut(&repo_id).unwrap();
            let replica_id = *next_replica_id;
            next_replica_id.0 += 1;
            replica_id
        };
        Ok(messages::CloneRepoResponse {
            repo_id,
            replica_id,
            credentials: RoomCredentials { name, token },
        })
    }

    async fn handle_sync_repo(
        self,
        _user: User,
        request: messages::SyncRepo,
    ) -> Result<messages::SyncRepoResponse> {
        let repo = self
            .db
            .repo(request.id)
            .ok_or_else(|| anyhow!("repo not found"))?;

        repo.read(|snapshot| {
            Ok(messages::SyncRepoResponse {
                operations: snapshot.operations_since(&(&request.max_operation_ids).into()),
                max_operation_ids: (&snapshot.max_operation_ids).into(),
            })
        })
    }

    async fn handle_publish_operations(
        self,
        _user: User,
        request: messages::PublishOperations,
    ) -> Result<()> {
        let repo = self
            .db
            .repo(request.repo_id)
            .ok_or_else(|| anyhow!("repo not found"))?;
        repo.update(|repo| {
            repo.apply_operations(request.operations);
            (None, ())
        });
        Ok(())
    }
}

#[derive(Clone)]
pub struct Db {
    kv: Arc<dyn KvStore>,
    snapshot: Arc<Mutex<DbSnapshot>>,
    local_operation_created: Option<Arc<dyn Send + Sync + Fn(RepoId, Operation)>>,
    repo_snapshot_changed: Option<Arc<dyn Send + Sync + Fn(RepoId, &RepoSnapshot)>>,
}

impl Db {
    fn new(kv: Arc<dyn KvStore>) -> Self {
        Self {
            kv,
            snapshot: Default::default(),
            local_operation_created: None,
            repo_snapshot_changed: None,
        }
    }

    fn on_local_operation(&mut self, callback: impl 'static + Send + Sync + Fn(RepoId, Operation)) {
        self.local_operation_created = Some(Arc::new(callback));
    }

    fn on_repo_snapshot_changed(
        &mut self,
        callback: impl 'static + Send + Sync + Fn(RepoId, &RepoSnapshot),
    ) {
        self.repo_snapshot_changed = Some(Arc::new(callback));
    }

    fn repo(&self, id: RepoId) -> Option<Repo> {
        self.snapshot
            .lock()
            .repos
            .contains_key(&id)
            .then_some(Repo {
                id,
                db: self.clone(),
            })
    }
}

#[derive(Clone)]
pub struct Repo {
    id: RepoId,
    db: Db,
}

impl Repo {
    fn create_empty_branch(&self, name: impl Into<Arc<str>>) -> Branch {
        let branch_id = self.update(|repo| {
            let (operation, branch_id) = repo.create_empty_branch(name);
            (Some(operation), branch_id)
        });
        Branch {
            id: branch_id,
            repo: self.clone(),
        }
    }

    fn load_branch(&self, name: &str) -> impl Future<Output = Result<Branch>> {
        let mut snapshot = self.read(|repo| repo.clone());
        let this = self.clone();
        let name = name.to_string();

        async move {
            let branch_id = *snapshot
                .branch_ids_by_name
                .load(&name, &*this.db.kv)
                .await?
                .ok_or_else(|| anyhow!("branch not found"))?;

            loop {
                let latest_snapshot = this.read(|repo| repo.clone());
                let operations = latest_snapshot.operations_since(&snapshot.max_operation_ids);
                if operations.is_empty() {
                    break;
                }
                snapshot.apply_operations(operations);
            }
            this.update(|latest_snapshot| {
                *latest_snapshot = snapshot;
                (None, ())
            });

            Ok(Branch {
                id: branch_id,
                repo: this,
            })
        }
    }

    fn branches(&self) -> Vec<Branch> {
        self.read(|repo| {
            repo.branches
                .iter()
                .map(|(branch_id, _)| Branch {
                    id: *branch_id,
                    repo: self.clone(),
                })
                .collect()
        })
    }

    fn read<F, T>(&self, f: F) -> T
    where
        F: FnOnce(&mut RepoSnapshot) -> T,
    {
        self.db
            .snapshot
            .lock()
            .repos
            .update(&self.id, |repo| f(repo))
            .expect("repo must exist")
    }

    fn update<F, T>(&self, f: F) -> T
    where
        F: FnOnce(&mut RepoSnapshot) -> (Option<Operation>, T),
    {
        self.db
            .snapshot
            .lock()
            .repos
            .update(&self.id, |repo| {
                let (operation, result) = f(repo);
                if let Some(operation) = operation {
                    repo.save_operation(operation.clone());
                    if let Some(local_operation_created) = self.db.local_operation_created.as_ref()
                    {
                        local_operation_created(self.id, operation);
                    }
                }

                if let Some(repo_snapshot_changed) = self.db.repo_snapshot_changed.as_ref() {
                    repo_snapshot_changed(self.id, repo);
                }

                result
            })
            .expect("repo must exist")
    }
}

#[derive(Clone)]
struct Branch {
    id: OperationId,
    repo: Repo,
}

impl Branch {
    pub fn name(&self) -> Arc<str> {
        self.repo
            .read(|repo| repo.branches.get(&self.id).unwrap().name.clone())
    }

    pub fn create_document(&self) -> Document {
        self.update(|document_id, parent, revision| {
            let operation = operations::CreateDocument {
                id: document_id,
                branch_id: self.id,
                parent,
            };
            operation.clone().apply(revision);
            let document = Document {
                id: document_id,
                branch: self.clone(),
            };

            (Operation::CreateDocument(operation), document)
        })
    }

    pub fn load_document(&self, id: OperationId) -> Result<Document> {
        self.read(|revision| {
            revision
                .document_metadata
                .get(&id)
                .ok_or_else(|| anyhow!("document not found"))?;
            Ok(Document {
                branch: self.clone(),
                id,
            })
        })
    }

    pub fn documents(&self) -> Vec<Document> {
        self.read(|revision| {
            revision
                .document_metadata
                .iter()
                .map(|(document_id, _)| Document {
                    branch: self.clone(),
                    id: *document_id,
                })
                .collect()
        })
    }

    fn update<F, T>(&self, f: F) -> T
    where
        F: FnOnce(OperationId, RevisionId, &mut Revision) -> (Operation, T),
    {
        self.repo.update(|repo| {
            let head = repo
                .branches
                .get(&self.id)
                .expect("branch must exist")
                .head
                .clone();
            let mut revision = repo.load_revision(&head).expect("revision must exist");
            let operation_id = repo.last_operation_id.tick();
            let (operation, result) = f(operation_id, head.clone(), &mut revision);
            repo.branches
                .update(&self.id, |branch| branch.head = operation_id.into());
            repo.revisions.insert(operation_id.into(), revision);
            (Some(operation), result)
        })
    }

    fn read<F, T>(&self, f: F) -> T
    where
        F: FnOnce(&Revision) -> T,
    {
        self.repo.read(|repo| {
            let head = repo
                .branches
                .get(&self.id)
                .expect("branch must exist")
                .head
                .clone();
            let revision = repo.load_revision(&head).expect("revision must exist");
            f(&revision)
        })
    }
}

#[derive(Clone, Default)]
struct DbSnapshot {
    repos: btree::Map<RepoId, RepoSnapshot>,
}

#[derive(Clone, Debug)]
struct DocumentMetadata {
    path: Option<Arc<Path>>,
    last_change: OperationId,
}

#[derive(Clone, Debug)]
struct DocumentFragment {
    document_id: OperationId,
    location: DenseId,
    insertion_id: OperationId,
    insertion_subrange: Range<usize>,
    tombstones: SmallVec<[Tombstone; 2]>,
    undo_count: u16,
}

impl DocumentFragment {
    fn is_sentinel(&self) -> bool {
        self.insertion_id == self.document_id
    }

    fn len(&self) -> usize {
        self.insertion_subrange.end - self.insertion_subrange.start
    }

    fn visible(&self) -> bool {
        self.undo_count % 2 == 0
            && self
                .tombstones
                .iter()
                .all(|tombstone| tombstone.undo_count % 2 == 1)
    }

    fn intersect(
        &self,
        range: AnchorRange,
    ) -> (
        Option<DocumentFragment>,
        DocumentFragment,
        Option<DocumentFragment>,
    ) {
        let mut intersection = self.clone();

        let prefix = if range.start_insertion_id == intersection.insertion_id
            && range.start_offset_in_insertion > intersection.insertion_subrange.start
        {
            let mut prefix = intersection.clone();
            prefix.insertion_subrange.end = range.start_offset_in_insertion;
            intersection.insertion_subrange.start = range.start_offset_in_insertion;
            Some(prefix)
        } else {
            None
        };

        let suffix = if range.end_insertion_id == intersection.insertion_id
            && range.end_offset_in_insertion > intersection.insertion_subrange.start
            && range.end_offset_in_insertion < intersection.insertion_subrange.end
        {
            let mut suffix = intersection.clone();
            suffix.insertion_subrange.start = range.end_offset_in_insertion;
            intersection.insertion_subrange.end = range.end_offset_in_insertion;
            Some(suffix)
        } else {
            None
        };

        (prefix, intersection, suffix)
    }
}

impl btree::Item for DocumentFragment {
    type Summary = DocumentFragmentSummary;

    fn summary(&self) -> DocumentFragmentSummary {
        DocumentFragmentSummary {
            visible_len: if self.visible() { self.len() } else { 0 },
            hidden_len: if self.visible() { 0 } else { self.len() },
            max_document_id: self.document_id,
            max_location: self.location.clone(),
        }
    }
}

#[derive(Eq, PartialEq, Clone, Debug, Default)]
pub struct DocumentFragmentSummary {
    visible_len: usize,
    hidden_len: usize,
    max_document_id: OperationId,
    max_location: DenseId,
}

impl btree::Summary for DocumentFragmentSummary {
    type Context = ();

    fn add_summary(&mut self, summary: &Self, _: &()) {
        debug_assert!(summary.max_document_id >= self.max_document_id);
        debug_assert!(
            summary.max_document_id > self.max_document_id
                || summary.max_location > self.max_location
        );

        self.visible_len += summary.visible_len;
        self.hidden_len += summary.hidden_len;
        self.max_document_id = summary.max_document_id;
        self.max_location = summary.max_location.clone();
    }
}

impl<'a> btree::Dimension<'a, DocumentFragmentSummary> for OperationId {
    fn add_summary(&mut self, summary: &'a DocumentFragmentSummary, _: &()) {
        *self = summary.max_document_id
    }
}

impl<'a> btree::SeekTarget<'a, DocumentFragmentSummary, DocumentFragmentSummary> for OperationId {
    fn seek_cmp(&self, cursor_location: &DocumentFragmentSummary, _: &()) -> Ordering {
        Ord::cmp(self, &cursor_location.max_document_id)
    }
}

impl<'a> btree::SeekTarget<'a, DocumentFragmentSummary, DocumentFragmentSummary>
    for (OperationId, &'a DenseId)
{
    fn seek_cmp(&self, cursor_location: &DocumentFragmentSummary, _: &()) -> Ordering {
        Ord::cmp(
            self,
            &(
                cursor_location.max_document_id,
                &cursor_location.max_location,
            ),
        )
    }
}

impl<'a> btree::SeekTarget<'a, DocumentFragmentSummary, DocumentFragmentSummary>
    for (OperationId, usize)
{
    fn seek_cmp(&self, cursor_location: &DocumentFragmentSummary, _: &()) -> Ordering {
        Ord::cmp(
            self,
            &(cursor_location.max_document_id, cursor_location.visible_len),
        )
    }
}

#[derive(Clone, Debug)]
struct Tombstone {
    id: OperationId,
    undo_count: u16,
}

#[derive(Clone, Debug)]
struct InsertionFragment {
    insertion_id: OperationId,
    offset_in_insertion: usize,
    fragment_location: DenseId,
}

impl InsertionFragment {
    fn new(fragment: &DocumentFragment) -> Self {
        Self {
            insertion_id: fragment.insertion_id,
            offset_in_insertion: fragment.insertion_subrange.start,
            fragment_location: fragment.location.clone(),
        }
    }
}

impl btree::Item for InsertionFragment {
    type Summary = InsertionFragmentSummary;

    fn summary(&self) -> Self::Summary {
        InsertionFragmentSummary {
            max_insertion_id: self.insertion_id,
            max_offset_in_insertion: self.offset_in_insertion,
        }
    }
}

impl btree::KeyedItem for InsertionFragment {
    type Key = InsertionFragmentSummary;

    fn key(&self) -> Self::Key {
        btree::Item::summary(self)
    }
}

#[derive(Clone, Default, Debug, Eq, PartialEq, PartialOrd, Ord)]
struct InsertionFragmentSummary {
    max_insertion_id: OperationId,
    max_offset_in_insertion: usize,
}

impl btree::Summary for InsertionFragmentSummary {
    type Context = ();

    fn add_summary(&mut self, summary: &Self, _: &()) {
        debug_assert!(summary.max_insertion_id >= self.max_insertion_id);
        debug_assert!(
            summary.max_insertion_id > self.max_insertion_id
                || summary.max_offset_in_insertion > self.max_offset_in_insertion
        );

        self.max_insertion_id = summary.max_insertion_id;
        self.max_offset_in_insertion = summary.max_offset_in_insertion;
    }
}

impl<'a> btree::SeekTarget<'a, InsertionFragmentSummary, InsertionFragmentSummary>
    for (OperationId, usize)
{
    fn seek_cmp(&self, cursor_location: &InsertionFragmentSummary, _: &()) -> Ordering {
        Ord::cmp(
            self,
            &(
                cursor_location.max_insertion_id,
                cursor_location.max_offset_in_insertion,
            ),
        )
    }
}

struct Document {
    branch: Branch,
    id: OperationId,
}

impl Document {
    pub fn edit<E, I, T>(&self, edits: E)
    where
        E: IntoIterator<IntoIter = I>,
        I: ExactSizeIterator<Item = (Range<usize>, T)>,
        T: Into<Arc<str>>,
    {
        self.branch.update(|operation_id, parent, revision| {
            let edits = edits.into_iter();
            let mut edit_op = operations::Edit {
                id: operation_id,
                document_id: self.id,
                branch_id: self.branch.id,
                parent: parent.clone(),
                edits: SmallVec::with_capacity(edits.len()),
            };
            let mut new_insertions = Vec::new();
            let mut insertion_offset = 0;

            let mut old_fragments = revision.document_fragments.cursor::<LocalEditDimension>();
            let mut new_fragments = old_fragments.slice(&self.id, Bias::Left, &());
            let document_visible_start = old_fragments.start().visible_len;
            let mut edits = edits
                .into_iter()
                .map(|(range, new_text)| {
                    (
                        (document_visible_start + range.start)
                            ..(document_visible_start + range.end),
                        new_text.into(),
                    )
                })
                .peekable();

            let mut new_ropes = RopeBuilder::new(
                revision.visible_text.cursor(0),
                revision.hidden_text.cursor(0),
            );
            new_fragments.append(
                old_fragments.slice(&(self.id, edits.peek().unwrap().0.start), Bias::Right, &()),
                &(),
            );

            new_ropes.append(
                new_fragments.summary().visible_len,
                new_fragments.summary().hidden_len,
            );

            let mut fragment_start = old_fragments.start().visible_len;
            for (range, new_text) in edits {
                let fragment_end = old_fragments.end(&()).visible_len;

                // If the current fragment ends before this range, then jump ahead to the first fragment
                // that extends past the start of this range, reusing any intervening fragments.
                if fragment_end < range.start {
                    // If the current fragment has been partially consumed, then consume the rest of it
                    // and advance to the next fragment before slicing.
                    if fragment_start > old_fragments.start().visible_len {
                        if fragment_end > fragment_start {
                            let mut suffix = old_fragments.item().unwrap().clone();
                            suffix.insertion_subrange.start +=
                                fragment_start - old_fragments.start().visible_len;
                            new_insertions
                                .push(btree::Edit::Insert(InsertionFragment::new(&suffix)));
                            new_ropes.push_fragment(&suffix, suffix.visible());
                            new_fragments.push(suffix, &());
                        }
                        old_fragments.next(&());
                    }

                    let slice = old_fragments.slice(&(self.id, range.start), Bias::Right, &());
                    new_ropes.append(slice.summary().visible_len, slice.summary().hidden_len);
                    new_fragments.append(slice, &());
                    fragment_start = old_fragments.start().visible_len;
                }

                let start_fragment = old_fragments.item().and_then(|item| {
                    if item.document_id == self.id {
                        Some(item)
                    } else {
                        None
                    }
                });
                let edit_start = if range.start == old_fragments.start().visible_len {
                    let start_fragment = old_fragments.prev_item().unwrap();
                    Anchor {
                        insertion_id: start_fragment.insertion_id,
                        offset_in_insertion: start_fragment.insertion_subrange.end,
                        bias: Bias::Right,
                    }
                } else {
                    let start_fragment = start_fragment.unwrap();
                    Anchor {
                        insertion_id: start_fragment.insertion_id,
                        offset_in_insertion: start_fragment.insertion_subrange.start
                            + (range.start - old_fragments.start().visible_len),
                        bias: Bias::Right,
                    }
                };

                // Preserve any portion of the current fragment that precedes this range.
                if fragment_start < range.start {
                    let mut prefix = old_fragments.item().unwrap().clone();
                    let prefix_len = range.start - fragment_start;
                    prefix.insertion_subrange.start +=
                        fragment_start - old_fragments.start().visible_len;
                    prefix.insertion_subrange.end = prefix.insertion_subrange.start + prefix_len;
                    prefix.location =
                        DenseId::between(&new_fragments.summary().max_location, &prefix.location);
                    new_insertions.push(btree::Edit::Insert(InsertionFragment::new(&prefix)));
                    new_ropes.push_fragment(&prefix, prefix.visible());
                    new_fragments.push(prefix, &());
                    fragment_start = range.start;
                }

                // Advance through every fragment that intersects this range, marking the intersecting
                // portions as deleted.
                while fragment_start < range.end {
                    let fragment = old_fragments.item().unwrap();
                    let fragment_end = old_fragments.end(&()).visible_len;
                    let mut intersection = fragment.clone();
                    let intersection_end = cmp::min(range.end, fragment_end);
                    if fragment.visible() {
                        let intersection_len = intersection_end - fragment_start;
                        intersection.insertion_subrange.start +=
                            fragment_start - old_fragments.start().visible_len;
                        intersection.insertion_subrange.end =
                            intersection.insertion_subrange.start + intersection_len;
                        intersection.location = DenseId::between(
                            &new_fragments.summary().max_location,
                            &intersection.location,
                        );
                        intersection.tombstones.push(Tombstone {
                            id: edit_op.id,
                            undo_count: 0,
                        });
                    }
                    if intersection.len() > 0 {
                        new_insertions
                            .push(btree::Edit::Insert(InsertionFragment::new(&intersection)));
                        new_ropes.push_fragment(&intersection, fragment.visible());
                        new_fragments.push(intersection, &());
                        fragment_start = intersection_end;
                    }
                    if fragment_end <= range.end {
                        old_fragments.next(&());
                    }
                }

                let end_fragment = old_fragments.item().and_then(|item| {
                    if item.document_id == self.id {
                        Some(item)
                    } else {
                        None
                    }
                });
                // Insert the new text before any existing fragments within the range.
                if !new_text.is_empty() {
                    let fragment = DocumentFragment {
                        document_id: self.id,
                        location: DenseId::between(
                            &new_fragments.summary().max_location,
                            end_fragment
                                .map_or(&DenseId::max(), |old_fragment| &old_fragment.location),
                        ),
                        insertion_id: edit_op.id,
                        insertion_subrange: insertion_offset..insertion_offset + new_text.len(),
                        tombstones: Default::default(),
                        undo_count: 0,
                    };
                    new_insertions.push(btree::Edit::Insert(InsertionFragment::new(&fragment)));
                    new_ropes.push_str(new_text.as_ref());
                    new_fragments.push(fragment, &());
                    insertion_offset += new_text.len();
                }

                let edit_end = if range.end == old_fragments.start().visible_len {
                    let end_fragment = old_fragments.prev_item().unwrap();
                    Anchor {
                        insertion_id: end_fragment.insertion_id,
                        offset_in_insertion: end_fragment.insertion_subrange.end,
                        bias: Bias::Left,
                    }
                } else {
                    let end_fragment = end_fragment.unwrap();
                    Anchor {
                        insertion_id: end_fragment.insertion_id,
                        offset_in_insertion: end_fragment.insertion_subrange.start
                            + (range.end - old_fragments.start().visible_len),
                        bias: Bias::Left,
                    }
                };
                edit_op.edits.push((
                    AnchorRange {
                        document_id: self.id,
                        start_insertion_id: edit_start.insertion_id,
                        start_offset_in_insertion: edit_start.offset_in_insertion,
                        start_bias: edit_start.bias,
                        end_insertion_id: edit_end.insertion_id,
                        end_offset_in_insertion: edit_end.offset_in_insertion,
                        end_bias: edit_end.bias,
                    },
                    new_text.clone(),
                ));
            }

            // If the current fragment has been partially consumed, then consume the rest of it
            // and advance to the next fragment before slicing.
            if fragment_start > old_fragments.start().visible_len {
                let fragment_end = old_fragments.end(&()).visible_len;
                if fragment_end > fragment_start {
                    let mut suffix = old_fragments.item().unwrap().clone();
                    let suffix_len = fragment_end - fragment_start;
                    suffix.insertion_subrange.start +=
                        fragment_start - old_fragments.start().visible_len;
                    suffix.insertion_subrange.end = suffix.insertion_subrange.start + suffix_len;
                    new_insertions.push(btree::Edit::Insert(InsertionFragment::new(&suffix)));
                    new_ropes.push_fragment(&suffix, suffix.visible());
                    new_fragments.push(suffix, &());
                }
                old_fragments.next(&());
            }

            let suffix = old_fragments.suffix(&());
            new_ropes.append(suffix.summary().visible_len, suffix.summary().hidden_len);
            new_fragments.append(suffix, &());
            let (visible_text, hidden_text) = new_ropes.finish();
            drop(old_fragments);

            revision.document_fragments = new_fragments;
            revision.insertion_fragments.edit(new_insertions, &());
            revision.visible_text = visible_text;
            revision.hidden_text = hidden_text;

            (Operation::Edit(edit_op), ())
        })
    }

    fn text(&self) -> Rope {
        self.branch.read(|revision| {
            revision
                .visible_text
                .slice(revision.document_visible_range(self.id))
        })
    }

    fn len(&self) -> usize {
        self.branch
            .read(|revision| revision.document_visible_range(self.id).len())
    }

    fn clip_offset(&self, offset: usize, bias: Bias) -> usize {
        self.branch.read(|revision| {
            let document_start = revision.document_visible_start(self.id);
            revision
                .visible_text
                .clip_offset(document_start + offset, bias)
                - document_start
        })
    }
}

#[derive(Clone, Debug, Default)]
pub struct LocalEditDimension {
    visible_len: usize,
    hidden_len: usize,
    max_document_id: OperationId,
}

impl<'a> btree::Dimension<'a, DocumentFragmentSummary> for LocalEditDimension {
    fn add_summary(&mut self, summary: &'a DocumentFragmentSummary, _: &()) {
        self.visible_len += summary.visible_len;
        self.hidden_len += summary.hidden_len;
        debug_assert!(summary.max_document_id >= self.max_document_id);
        self.max_document_id = summary.max_document_id;
    }
}

impl<'a> btree::SeekTarget<'a, DocumentFragmentSummary, LocalEditDimension> for OperationId {
    fn seek_cmp(&self, cursor_location: &LocalEditDimension, _: &()) -> Ordering {
        Ord::cmp(self, &cursor_location.max_document_id)
    }
}

impl<'a> btree::SeekTarget<'a, DocumentFragmentSummary, LocalEditDimension> for usize {
    fn seek_cmp(&self, cursor_location: &LocalEditDimension, _: &()) -> Ordering {
        Ord::cmp(self, &cursor_location.visible_len)
    }
}

impl<'a> btree::SeekTarget<'a, DocumentFragmentSummary, LocalEditDimension>
    for (OperationId, usize)
{
    fn seek_cmp(&self, cursor_location: &LocalEditDimension, _: &()) -> Ordering {
        Ord::cmp(
            self,
            &(cursor_location.max_document_id, cursor_location.visible_len),
        )
    }
}

#[derive(Copy, Clone, Debug)]
struct Anchor {
    insertion_id: OperationId,
    offset_in_insertion: usize,
    bias: Bias,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct AnchorRange {
    document_id: OperationId,
    start_insertion_id: OperationId,
    start_offset_in_insertion: usize,
    #[serde(with = "bias_serialization")]
    start_bias: Bias,
    end_insertion_id: OperationId,
    end_offset_in_insertion: usize,
    #[serde(with = "bias_serialization")]
    end_bias: Bias,
}

impl AnchorRange {
    fn start(&self) -> Anchor {
        Anchor {
            insertion_id: self.start_insertion_id,
            offset_in_insertion: self.start_offset_in_insertion,
            bias: self.start_bias,
        }
    }

    fn end(&self) -> Anchor {
        Anchor {
            insertion_id: self.end_insertion_id,
            offset_in_insertion: self.end_offset_in_insertion,
            bias: self.end_bias,
        }
    }
}

mod bias_serialization {
    use crate::btree::Bias;
    use serde::{Deserialize, Deserializer, Serialize, Serializer};

    pub fn serialize<S>(field: &Bias, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        match field {
            Bias::Left => "left".serialize(serializer),
            Bias::Right => "right".serialize(serializer),
        }
    }

    pub fn deserialize<'de, D>(deserializer: D) -> Result<Bias, D::Error>
    where
        D: Deserializer<'de>,
    {
        match String::deserialize(deserializer)?.as_str() {
            "left" => Ok(Bias::Left),
            "right" => Ok(Bias::Right),
            _ => Err(serde::de::Error::custom("invalid bias")),
        }
    }
}

struct RopeBuilder<'a> {
    old_visible_cursor: rope::Cursor<'a>,
    old_hidden_cursor: rope::Cursor<'a>,
    new_visible: Rope,
    new_hidden: Rope,
}

impl<'a> RopeBuilder<'a> {
    fn new(old_visible_cursor: rope::Cursor<'a>, old_hidden_cursor: rope::Cursor<'a>) -> Self {
        Self {
            old_visible_cursor,
            old_hidden_cursor,
            new_visible: Rope::new(),
            new_hidden: Rope::new(),
        }
    }

    fn append(&mut self, visible_len: usize, hidden_len: usize) {
        self.push(visible_len, true, true);
        self.push(hidden_len, false, false);
    }

    fn push_fragment(&mut self, fragment: &DocumentFragment, was_visible: bool) {
        self.push(fragment.len(), was_visible, fragment.visible())
    }

    fn push(&mut self, len: usize, was_visible: bool, is_visible: bool) {
        let text = if was_visible {
            self.old_visible_cursor
                .slice(self.old_visible_cursor.offset() + len as usize)
        } else {
            self.old_hidden_cursor
                .slice(self.old_hidden_cursor.offset() + len)
        };
        if is_visible {
            self.new_visible.append(text);
        } else {
            self.new_hidden.append(text);
        }
    }

    fn push_str(&mut self, text: &str) {
        self.new_visible.push(text);
    }

    fn finish(mut self) -> (Rope, Rope) {
        self.new_visible.append(self.old_visible_cursor.suffix());
        self.new_hidden.append(self.old_hidden_cursor.suffix());
        (self.new_visible, self.new_hidden)
    }
}

#[derive(Clone, Debug)]
pub struct RepoSnapshot {
    last_operation_id: OperationId,
    branches: btree::Map<OperationId, BranchSnapshot>,
    // TODO: Change String to Arc<str> for branch_ids_by_name
    branch_ids_by_name: btree::Map<String, OperationId>,
    operations: btree::Map<OperationId, Operation>,
    revisions: btree::Map<RevisionId, Revision>,
    max_operation_ids: btree::Map<ReplicaId, OperationCount>,
    deferred_operations: btree::Sequence<DeferredOperation>,
}

#[derive(Serialize, Deserialize)]
struct SavedRepoSnapshot {
    last_operation_id: OperationId,
    branches: btree::SavedId,
    branch_ids_by_name: btree::SavedId,
    operations: btree::SavedId,
    max_operation_ids: btree::SavedId,
    deferred_operations: btree::SavedId,
}

impl Default for RepoSnapshot {
    fn default() -> Self {
        Self {
            last_operation_id: Default::default(),
            branches: Default::default(),
            branch_ids_by_name: Default::default(),
            operations: Default::default(),
            revisions: btree::Map::from_ordered_entries([(
                RevisionId::default(),
                Revision::default(),
            )]),
            max_operation_ids: Default::default(),
            deferred_operations: Default::default(),
        }
    }
}

impl RepoSnapshot {
    fn new(replica_id: ReplicaId) -> Self {
        Self {
            last_operation_id: OperationId::new(replica_id),
            ..Default::default()
        }
    }

    async fn load(id: RepoId, kv: &dyn KvStore) -> Result<Self> {
        let repo_bytes = kv
            .load(btree::namespace_bytes("repo"), id.0.as_u128())
            .await?;
        let saved_repo = serde_bare::from_slice::<SavedRepoSnapshot>(&repo_bytes)?;
        Ok(Self {
            last_operation_id: saved_repo.last_operation_id,
            branches: btree::Map::load_root(saved_repo.branches, kv).await?,
            branch_ids_by_name: btree::Map::load_root(saved_repo.branch_ids_by_name, kv).await?,
            operations: btree::Map::load_root(saved_repo.operations, kv).await?,
            revisions: Default::default(),
            max_operation_ids: btree::Map::load_root(saved_repo.max_operation_ids, kv).await?,
            deferred_operations: btree::Sequence::load_root(saved_repo.deferred_operations, kv)
                .await?,
        })
    }

    async fn save(&self, id: RepoId, kv: &dyn KvStore) -> Result<()> {
        let saved_repo = SavedRepoSnapshot {
            last_operation_id: self.last_operation_id,
            branches: self.branches.save(kv).await?,
            branch_ids_by_name: self.branch_ids_by_name.save(kv).await?,
            operations: self.operations.save(kv).await?,
            max_operation_ids: self.max_operation_ids.save(kv).await?,
            deferred_operations: self.deferred_operations.save(kv).await?,
        };
        let repo_bytes = serde_bare::to_vec(&saved_repo)?;
        kv.store(btree::namespace_bytes("repo"), id.0.as_u128(), repo_bytes)
            .await?;
        Ok(())
    }

    fn create_empty_branch(&mut self, name: impl Into<Arc<str>>) -> (Operation, OperationId) {
        let name = name.into();
        let branch_id = self.last_operation_id.tick();
        self.branches.insert(
            branch_id,
            BranchSnapshot {
                name: name.clone(),
                head: branch_id.into(),
            },
        );

        (
            Operation::CreateBranch(operations::CreateBranch {
                id: branch_id,
                name,
                parent: Default::default(),
            }),
            branch_id,
        )
    }

    fn operations_since(&self, version: &btree::Map<ReplicaId, OperationCount>) -> Vec<Operation> {
        let mut new_operations = Vec::new();
        for (replica_id, end_op_count) in self.max_operation_ids.iter() {
            let end_op = OperationId {
                replica_id: *replica_id,
                operation_count: *end_op_count,
            };
            if let Some(start_op_count) = version.get(&replica_id) {
                let start_op = OperationId {
                    replica_id: *replica_id,
                    operation_count: *start_op_count,
                };
                new_operations.extend(
                    self.operations
                        .range((Bound::Excluded(&start_op), Bound::Included(&end_op)))
                        .map(|(_, op)| op.clone()),
                );
            } else {
                let start_op = OperationId::new(*replica_id);
                new_operations.extend(
                    self.operations
                        .range((Bound::Included(&start_op), Bound::Included(&end_op)))
                        .map(|(_, op)| op.clone()),
                );
            }
        }
        new_operations
    }

    /// Apply the given operations and any deferred operations that are now applicable.
    fn apply_operations(&mut self, operations: impl Into<VecDeque<Operation>>) {
        let mut operations = operations.into();
        while let Some(operation) = operations.pop_front() {
            if self.operations.contains_key(&operation.id()) {
                continue;
            }

            if operation
                .parent()
                .iter()
                .all(|parent| self.operations.contains_key(&parent))
            {
                let operation_id = operation.id();
                let mut new_head;
                match &operation {
                    Operation::CreateBranch(op) => {
                        self.branches.insert(
                            op.id,
                            BranchSnapshot {
                                name: op.name.clone(),
                                head: op.id.into(),
                            },
                        );
                        new_head = RevisionId::from(op.id);
                    }
                    Operation::CreateDocument(operations::CreateDocument {
                        branch_id,
                        parent,
                        ..
                    })
                    | Operation::Edit(operations::Edit {
                        branch_id, parent, ..
                    }) => {
                        if let Some(branch) = self.branches.get(branch_id).cloned() {
                            new_head = branch.head;
                            new_head.observe(operation_id, &parent);
                            self.branches
                                .update(&branch_id, |branch| branch.head = new_head.clone());
                        } else {
                            log::error!(
                                "could not apply operation {:?}: branch {:?} does not exist",
                                operation,
                                branch_id
                            );
                            continue;
                        }
                    }
                };

                self.save_operation(operation);
                self.flush_deferred_operations(operation_id, &mut operations);

                // The following ensures that a revision for the branch head is always present.
                #[cfg(not(any(test, feature = "test-support")))]
                if let Err(error) = self.load_revision(&new_head) {
                    log::error!(
                        "could not create revision for head {:?}: {:?}",
                        new_head,
                        error
                    );
                    continue;
                }
                #[cfg(any(test, feature = "test-support"))]
                self.load_revision(&new_head).unwrap();
            } else {
                for parent in operation.parent().iter() {
                    self.deferred_operations.insert_or_replace(
                        DeferredOperation {
                            parent: *parent,
                            operation: operation.clone(),
                        },
                        &(),
                    );
                }
            }
        }
    }

    /// Remove any operations deferred on the given parent and add them to the
    /// provided operation queue. This is called in `apply_operations`.
    fn flush_deferred_operations(
        &mut self,
        parent_id: OperationId,
        operations: &mut VecDeque<Operation>,
    ) {
        let mut cursor = self.deferred_operations.cursor::<OperationId>();
        let mut remaining = cursor.slice(&parent_id, Bias::Left, &());
        operations.extend(
            cursor
                .slice(&parent_id, Bias::Right, &())
                .iter()
                .map(|deferred| deferred.operation.clone()),
        );
        remaining.append(cursor.suffix(&()), &());
        drop(cursor);
        self.deferred_operations = remaining;
    }

    fn save_operation(&mut self, operation: Operation) {
        let replica_id = operation.id().replica_id;
        let count = operation.id().operation_count;
        if self.max_operation_ids.get(&replica_id).copied() < Some(count) {
            self.max_operation_ids.insert(replica_id, count);
        }
        self.last_operation_id.observe(operation.id());
        self.operations.insert(operation.id(), operation);
    }

    fn operation(&self, operation_id: OperationId) -> Option<&Operation> {
        self.operations.get(&operation_id)
    }

    fn load_revision(&mut self, revision_id: &RevisionId) -> Result<Revision> {
        // First, check if we have a revision cached for this revision id.
        // If not, we'll need to reconstruct it from a previous revision.
        // We need to find a cached revision that is an ancestor of the given revision id.
        // Once we find it, we must apply all ancestors of the given revision id that are not contained in the cached revision.
        if let Some(revision) = self.revisions.get(revision_id) {
            Ok(revision.clone())
        } else {
            struct Search<'a> {
                start: OperationId,
                ancestor: &'a RevisionId,
            }

            let mut ancestors = HashMap::<&RevisionId, HashSet<OperationId>>::default();
            let mut searches = VecDeque::new();
            let mut operations = BTreeSet::new();
            for operation_id in revision_id.iter() {
                operations.insert((operation_id.operation_count, operation_id.replica_id));
                searches.push_back(Search {
                    start: *operation_id,
                    ancestor: self
                        .operation(*operation_id)
                        .ok_or_else(|| anyhow!("operation {:?} not found", operation_id))?
                        .parent(),
                });
            }

            let mut common_ancestor_revision = Revision::default();
            let mut missing_operations_start = (OperationCount::default(), ReplicaId::default());
            while let Some(search) = searches.pop_front() {
                let reachable_from = ancestors.entry(search.ancestor).or_default();
                reachable_from.insert(search.start);

                // If the current revision is reachable from every operation in the original
                // revision id, it's a common ancestor.
                if reachable_from.len() == revision_id.len() {
                    if let Some(revision) = self.revisions.get(search.ancestor) {
                        // We've found a cached revision for a common ancestor. For it to
                        // be a common ancestor means that all its downstream operations must
                        // have causally happened after it. Therefore, we should be able to
                        // use the maximum lamport timestamp in the common ancestor's revision
                        // and select only those operations we've found in the backwards search
                        // which have a higher lamport timestamp.
                        common_ancestor_revision = revision.clone();
                        if let Some(max_operation_count) = search
                            .ancestor
                            .iter()
                            .map(|operation_id| operation_id.operation_count)
                            .max()
                        {
                            missing_operations_start = (
                                OperationCount(max_operation_count.0 + 1),
                                ReplicaId::default(),
                            );
                        }
                        break;
                    }
                }

                for operation_id in search.ancestor.iter() {
                    operations.insert((operation_id.operation_count, operation_id.replica_id));
                    searches.push_back(Search {
                        start: search.start,
                        ancestor: self
                            .operation(*operation_id)
                            .expect("operation must exist")
                            .parent(),
                    });
                }
            }

            // Apply all the missing operations to the found revision.
            for (operation_count, replica_id) in operations.range(missing_operations_start..) {
                let missing_operation_id = OperationId {
                    replica_id: *replica_id,
                    operation_count: *operation_count,
                };
                match self
                    .operation(missing_operation_id)
                    .expect("operation must exist")
                    .clone()
                {
                    Operation::CreateDocument(op) => {
                        op.apply(&mut common_ancestor_revision);
                    }
                    Operation::Edit(op) => {
                        let parent_revision = self.load_revision(&op.parent)?;
                        op.apply(&parent_revision, &mut common_ancestor_revision)?;
                    }
                    Operation::CreateBranch(_) => {
                        // Creating a branch doesn't have an impact on the revision, so we
                        // can ignore it.
                    }
                }
            }

            self.revisions
                .insert(revision_id.clone(), common_ancestor_revision.clone());
            Ok(common_ancestor_revision)
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct DeferredOperation {
    parent: OperationId,
    operation: Operation,
}

impl PartialEq for DeferredOperation {
    fn eq(&self, other: &Self) -> bool {
        self.parent == other.parent && self.operation.id() == other.operation.id()
    }
}

impl Eq for DeferredOperation {}

impl PartialOrd for DeferredOperation {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for DeferredOperation {
    fn cmp(&self, other: &Self) -> Ordering {
        self.parent
            .cmp(&other.parent)
            .then_with(|| self.operation.id().cmp(&other.operation.id()))
    }
}

impl btree::Item for DeferredOperation {
    type Summary = OperationId;

    fn summary(&self) -> Self::Summary {
        self.parent
    }
}

impl btree::KeyedItem for DeferredOperation {
    type Key = (OperationId, OperationId);

    fn key(&self) -> Self::Key {
        (self.parent, self.operation.id())
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct BranchSnapshot {
    name: Arc<str>,
    head: RevisionId,
}

#[derive(Default, Debug, Clone)]
pub struct Revision {
    document_metadata: btree::Map<OperationId, DocumentMetadata>,
    document_fragments: btree::Sequence<DocumentFragment>,
    insertion_fragments: btree::Sequence<InsertionFragment>,
    visible_text: Rope,
    hidden_text: Rope,
}

impl Revision {
    /// Return the locations of all document fragments for a given insertion and
    /// subrange of that insertion.
    fn fragment_locations(
        &self,
        insertion_id: OperationId,
        insertion_subrange: Range<usize>,
    ) -> impl Iterator<Item = &DenseId> {
        let mut cursor = self
            .insertion_fragments
            .cursor::<InsertionFragmentSummary>();
        cursor.seek(&(insertion_id, insertion_subrange.start), Bias::Left, &());

        // Avoid overshooting the last fragment.
        if cursor
            .item()
            .map_or(true, |item| item.insertion_id > insertion_id)
        {
            cursor.prev(&());
        }

        cursor
            .take_while(move |item| {
                item.insertion_id == insertion_id
                    && item.offset_in_insertion < insertion_subrange.end
            })
            .map(|item| &item.fragment_location)
    }

    fn visible_fragments_for_range(
        &self,
        range: AnchorRange,
    ) -> Result<impl Iterator<Item = &DocumentFragment>> {
        let start_fragment_id = self
            .fragment_locations(
                range.start_insertion_id,
                range.start_offset_in_insertion..usize::MAX,
            )
            .next()
            .ok_or_else(|| {
                anyhow!(
                    "start fragment not found. start_insertion_id: {:?}, start_offset_in_insertion: {}",
                    range.start_insertion_id,
                    range.start_offset_in_insertion,
                )
            })?;
        let mut cursor = self.document_fragments.cursor::<DocumentFragmentSummary>();
        cursor.seek(&(range.document_id, start_fragment_id), Bias::Left, &());
        Ok(std::iter::from_fn(move || {
            let fragment = cursor.item()?;
            if fragment.document_id != range.document_id {
                return None;
            }

            let next_visible_ix = cursor.end(&()).visible_len;
            cursor.seek(&(range.document_id, next_visible_ix), Bias::Right, &());
            Some(fragment)
        }))
    }

    fn document_visible_start(&self, document_id: OperationId) -> usize {
        let mut fragments = self.document_fragments.cursor::<LocalEditDimension>();
        fragments.seek(&document_id, Bias::Left, &());
        fragments.start().visible_len
    }

    fn document_visible_range(&self, document_id: OperationId) -> Range<usize> {
        let mut fragments = self.document_fragments.cursor::<LocalEditDimension>();
        fragments.seek(&document_id, Bias::Left, &());
        let start = fragments.start().visible_len;

        let mut next_doc_id = document_id;
        next_doc_id.operation_count.0 += 1;
        fragments.seek(&next_doc_id, Bias::Left, &());
        let end = fragments.start().visible_len;

        start..end
    }

    fn check_invariants(&self) {
        let mut insertion_fragments = self.insertion_fragments.iter().peekable();
        while let Some(insertion_fragment) = insertion_fragments.next() {
            if let Some(next_insertion_fragment) = insertion_fragments.peek() {
                if next_insertion_fragment.insertion_id == insertion_fragment.insertion_id {
                    assert!(
                        insertion_fragment.fragment_location
                            < next_insertion_fragment.fragment_location
                    );
                }
            }
        }

        for document_fragment in self.document_fragments.iter() {
            assert!(
                document_fragment.is_sentinel() || !document_fragment.insertion_subrange.is_empty()
            );
        }
    }
}

#[cfg(test)]
#[ctor::ctor]
fn init_logger() {
    if std::env::var("RUST_LOG").is_ok() {
        env_logger::init();
    } else {
        env_logger::builder()
            .filter_level(log::LevelFilter::Error)
            .init();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test::{TestClientNetwork, TestNetwork, TestServerNetwork};
    use gpui::executor::{Background, Deterministic};
    use rand::prelude::*;
    use std::{
        env, fs,
        ops::Deref,
        path::PathBuf,
        sync::atomic::{AtomicBool, Ordering::SeqCst},
    };
    use util::{path_env_var, post_inc};

    struct TestKv {
        executor: Arc<Background>,
        kv: Arc<btree::tests::InMemoryKv>,
    }

    impl TestKv {
        fn new(executor: Arc<Background>) -> Self {
            Self {
                executor,
                kv: Default::default(),
            }
        }
    }

    impl KvStore for TestKv {
        fn load(&self, namespace: [u8; 16], key: u128) -> BoxFuture<Result<Vec<u8>>> {
            let kv = self.kv.clone();
            let executor = self.executor.clone();
            async move {
                executor.simulate_random_delay().await;
                kv.load(namespace, key).await
            }
            .boxed()
        }

        fn store(&self, namespace: [u8; 16], key: u128, value: Vec<u8>) -> BoxFuture<Result<()>> {
            let kv = self.kv.clone();
            let executor = self.executor.clone();
            async move {
                executor.simulate_random_delay().await;
                kv.store(namespace, key, value).await
            }
            .boxed()
        }
    }

    #[gpui::test]
    async fn test_basic_collaboration(deterministic: Arc<Deterministic>) {
        let network = TestNetwork::new(deterministic.build_background());
        let server_kv = Arc::new(TestKv::new(deterministic.build_background()));
        let server = Server::new(network.server(), server_kv);

        let kv_a = Arc::new(TestKv::new(deterministic.build_background()));
        let client_a = Client::new(
            deterministic.build_background(),
            network.client("client-a"),
            kv_a.clone(),
        );
        let repo_a = client_a.create_repo();
        let branch_a = repo_a.create_empty_branch("main");

        let doc1_a = branch_a.create_document();
        doc1_a.edit([(0..0, "abc")]);

        let doc2_a = branch_a.create_document();
        doc2_a.edit([(0..0, "def")]);

        assert_eq!(doc1_a.text().to_string(), "abc");
        assert_eq!(doc2_a.text().to_string(), "def");

        let kv_b = Arc::new(TestKv::new(deterministic.build_background()));
        client_a.publish_repo(&repo_a, "repo-1").await.unwrap();
        let client_b = Client::new(
            deterministic.build_background(),
            network.client("client-b"),
            kv_b,
        );
        let repo_b = client_b.clone_repo("repo-1").await.unwrap();
        deterministic.run_until_parked();
        let branch_b = repo_b.load_branch("main").await.unwrap();

        let doc1_b = branch_b.load_document(doc1_a.id).unwrap();
        let doc2_b = branch_b.load_document(doc2_a.id).unwrap();
        assert_eq!(doc1_b.text().to_string(), "abc");
        assert_eq!(doc2_b.text().to_string(), "def");

        doc1_a.edit([(1..2, "ghi")]);
        assert_eq!(doc1_a.text().to_string(), "aghic");
        assert_eq!(doc1_b.text().to_string(), "abc");

        deterministic.run_until_parked();

        assert_eq!(doc1_a.text().to_string(), "aghic");
        assert_eq!(doc1_b.text().to_string(), "aghic");

        drop(client_a);
        let client_a2 = Client::new(
            deterministic.build_background(),
            network.client("client-a"),
            kv_a,
        );
        let repo_a2 = client_a2.repo(repo_b.id).await.unwrap();
    }

    #[derive(Clone)]
    struct StoredOperation {
        operation: TestOperation,
        applied: Arc<AtomicBool>,
    }

    #[derive(Default)]
    struct TestPlan {
        operations: Vec<StoredOperation>,
    }

    impl TestPlan {
        fn load(&mut self, path: &Path) {
            let plan = fs::read_to_string(path).unwrap();
            self.operations = gpui::serde_json::from_str::<Vec<TestOperation>>(&plan)
                .unwrap()
                .into_iter()
                .map(|operation| StoredOperation {
                    operation,
                    applied: Default::default(),
                })
                .collect();
        }

        fn save(&self, path: &Path) {
            // Format each operation as one line
            let mut json = Vec::new();
            json.push(b'[');
            for stored_operation in &self.operations {
                if !stored_operation.applied.load(SeqCst) {
                    continue;
                }
                if json.len() > 1 {
                    json.push(b',');
                }
                json.extend_from_slice(b"\n  ");
                gpui::serde_json::to_writer(&mut json, &stored_operation.operation).unwrap();
            }
            json.extend_from_slice(b"\n]\n");

            fs::write(path, json).unwrap();
        }
    }

    lazy_static::lazy_static! {
        static ref PLAN_LOAD_PATH: Option<PathBuf> = path_env_var("LOAD_PLAN");
        static ref PLAN_SAVE_PATH: Option<PathBuf> = path_env_var("SAVE_PLAN");
        static ref PLAN: Mutex<TestPlan> = Default::default();
    }

    fn on_failure() {
        if let Some(path) = &*PLAN_SAVE_PATH {
            eprintln!("saved test plan to path {:?}", path);
            PLAN.lock().save(path);
        }
    }

    #[gpui::test(iterations = 100, on_failure = "on_failure")]
    async fn test_random_collaboration(deterministic: Arc<Deterministic>, mut rng: StdRng) {
        deterministic.forbid_parking();

        let max_peers = env::var("MAX_PEERS")
            .map(|i| i.parse().expect("invalid `MAX_PEERS` variable"))
            .unwrap_or(3);
        let max_operations = env::var("OPERATIONS")
            .map(|i| i.parse().expect("invalid `OPERATIONS` variable"))
            .unwrap_or(10);

        let kv = Arc::new(TestKv::new(deterministic.build_background()));
        let network = TestNetwork::new(deterministic.build_background());
        let server = Server::new(network.server(), kv.clone());
        let mut clients: Vec<TestClient> = Vec::new();
        let mut next_client_id = 0;
        let mut next_branch_name = 0;

        if let Some(path) = &*PLAN_LOAD_PATH {
            PLAN.lock().load(path);
            for stored_operation in PLAN.lock().operations.clone() {
                log::info!("{:?}", stored_operation.operation);
                if stored_operation
                    .operation
                    .apply(&deterministic, &kv, &mut clients, &network)
                    .await
                    .is_ok()
                {
                    stored_operation.applied.store(true, SeqCst);
                }
            }
        } else {
            for _ in 0..max_operations {
                let operation = TestOperation::generate(
                    max_peers,
                    &mut rng,
                    &server,
                    &clients,
                    &mut next_client_id,
                    &mut next_branch_name,
                );

                PLAN.lock().operations.push(StoredOperation {
                    operation: operation.clone(),
                    applied: Arc::new(AtomicBool::new(true)),
                });
                log::info!("{:?}", operation);

                operation
                    .apply(&deterministic, &kv, &mut clients, &network)
                    .await
                    .unwrap();
            }
        }

        log::info!("Quiescing");
        deterministic.run_until_parked();
        for client in clients {
            for client_repo in client.repos() {
                if !client.is_local_repo(&client_repo) {
                    let server_repo = server.db.repo(client_repo.id).unwrap();
                    let client_branches = client_repo.branches();
                    let server_branches = server_repo.branches();
                    assert_eq!(
                        client_branches
                            .iter()
                            .map(|branch| branch.id)
                            .collect::<Vec<_>>(),
                        server_branches
                            .iter()
                            .map(|branch| branch.id)
                            .collect::<Vec<_>>(),
                        "client {} branches != server branches",
                        client.id
                    );
                }
            }
        }
    }

    struct TestClient {
        id: usize,
        client: Arc<Client<Arc<Background>, TestClientNetwork>>,
    }

    impl Deref for TestClient {
        type Target = Arc<Client<Arc<Background>, TestClientNetwork>>;

        fn deref(&self) -> &Self::Target {
            &self.client
        }
    }

    #[derive(Debug, Clone, Serialize, Deserialize)]
    enum TestOperation {
        Yield {
            count: usize,
        },
        Quiesce,
        AddClient {
            id: usize,
            name: String,
        },
        CreateRepo {
            client_id: usize,
        },
        PublishRepo {
            client_id: usize,
            id: RepoId,
            name: Arc<str>,
        },
        CloneRepo {
            client_id: usize,
            name: Arc<str>,
        },
        CreateEmptyBranch {
            client_id: usize,
            repo_id: RepoId,
            name: String,
        },
        CreateDocument {
            client_id: usize,
            repo_id: RepoId,
            branch_name: Arc<str>,
        },
        EditDocument {
            client_id: usize,
            repo_id: RepoId,
            branch_name: Arc<str>,
            document_id: OperationId,
            edits: Vec<(Range<usize>, String)>,
        },
    }

    impl TestOperation {
        fn generate(
            max_peers: usize,
            rng: &mut StdRng,
            server: &Server<TestServerNetwork>,
            clients: &[TestClient],
            next_client_id: &mut usize,
            next_branch_name: &mut usize,
        ) -> Self {
            if rng.gen_bool(0.2) {
                if rng.gen() {
                    let count = rng.gen_range(1..=5);
                    Self::Yield { count }
                } else {
                    Self::Quiesce
                }
            } else if clients.is_empty() || (clients.len() < max_peers && rng.gen_bool(0.1)) {
                let client_id = post_inc(next_client_id);
                Self::AddClient {
                    id: client_id,
                    name: format!("client-{}", client_id),
                }
            } else {
                let client = clients.choose(rng).unwrap();
                let client_id = client.id;
                let repos = client.repos();
                if repos.is_empty() || rng.gen_bool(0.1) {
                    let mut server_repo_ids_by_name = server.repo_ids_by_name.lock().clone();

                    // Avoid cloning repos that were already published/cloned by this client.
                    let client_repo_ids = repos.iter().map(|repo| repo.id).collect::<HashSet<_>>();
                    server_repo_ids_by_name.retain(|_, id| !client_repo_ids.contains(id));

                    if server_repo_ids_by_name.is_empty() || rng.gen_bool(0.1) {
                        Self::CreateRepo { client_id }
                    } else {
                        let repo_name = server_repo_ids_by_name.into_keys().choose(rng).unwrap();
                        Self::CloneRepo {
                            client_id,
                            name: repo_name,
                        }
                    }
                } else {
                    let repo = repos.choose(rng).unwrap().clone();
                    let branches = repo.branches();
                    if client.is_local_repo(&repo) && rng.gen_bool(0.05) {
                        Self::PublishRepo {
                            client_id,
                            id: repo.id,
                            name: format!("repo-{}", repo.id).into(),
                        }
                    } else if branches.is_empty() || rng.gen_bool(0.1) {
                        Self::CreateEmptyBranch {
                            client_id,
                            repo_id: repo.id,
                            name: format!("branch-{}", post_inc(next_branch_name)),
                        }
                    } else {
                        let branch = branches.choose(rng).unwrap().clone();
                        let documents = branch.documents();
                        if documents.is_empty() || rng.gen_bool(0.1) {
                            Self::CreateDocument {
                                client_id,
                                repo_id: repo.id,
                                branch_name: branch.name(),
                            }
                        } else {
                            let document = documents.choose(rng).unwrap().clone();
                            let end = document
                                .clip_offset(rng.gen_range(0..=document.len()), Bias::Right);
                            let start = document.clip_offset(rng.gen_range(0..=end), Bias::Left);
                            let new_text_len = rng.gen_range(0..=3);
                            let text = util::RandomCharIter::new(rng)
                                .take(new_text_len)
                                .collect::<String>();
                            Self::EditDocument {
                                client_id,
                                repo_id: repo.id,
                                branch_name: branch.name(),
                                document_id: document.id,
                                edits: vec![(start..end, text)],
                            }
                        }
                    }
                }
            }
        }

        async fn apply(
            &self,
            deterministic: &Arc<Deterministic>,
            kv: &Arc<TestKv>,
            clients: &mut Vec<TestClient>,
            network: &TestNetwork,
        ) -> Result<()> {
            match self {
                TestOperation::Yield { count } => {
                    for _ in 0..*count {
                        smol::future::yield_now().await;
                    }
                }

                TestOperation::Quiesce => {
                    deterministic.run_until_parked();
                }

                TestOperation::AddClient { id, name } => {
                    let client = Client::new(
                        deterministic.build_background(),
                        network.client(name.as_str()),
                        kv.clone(),
                    );
                    clients.push(TestClient { id: *id, client });
                }

                TestOperation::CreateRepo { client_id } => {
                    let client = clients
                        .iter_mut()
                        .find(|c| c.id == *client_id)
                        .ok_or_else(|| anyhow!("client not found"))?;
                    client.create_repo();
                }

                TestOperation::PublishRepo {
                    client_id,
                    id,
                    name,
                } => {
                    let client = clients
                        .iter_mut()
                        .find(|c| c.id == *client_id)
                        .ok_or_else(|| anyhow!("client not found"))?;
                    let repo = client.repo(*id).await?;
                    let publish = client.publish_repo(&repo, name.clone());
                    let publish = async move {
                        publish.await.log_err();
                    };
                    deterministic.build_background().spawn(publish);
                }

                TestOperation::CloneRepo { client_id, name } => {
                    let client = clients
                        .iter_mut()
                        .find(|c| c.id == *client_id)
                        .ok_or_else(|| anyhow!("client not found"))?;
                    let clone = client.clone_repo(name.clone());
                    let clone = async move {
                        clone.await.log_err();
                    };
                    deterministic.build_background().spawn(clone);
                }

                TestOperation::CreateEmptyBranch {
                    client_id,
                    repo_id,
                    name,
                } => {
                    let client = clients
                        .iter_mut()
                        .find(|c| c.id == *client_id)
                        .ok_or_else(|| anyhow!("client not found"))?;
                    let repo = client.repo(*repo_id).await?;
                    repo.create_empty_branch(name.as_str());
                }

                TestOperation::CreateDocument {
                    client_id,
                    repo_id,
                    branch_name,
                } => {
                    let client = clients
                        .iter_mut()
                        .find(|c| c.id == *client_id)
                        .ok_or_else(|| anyhow!("client not found"))?;
                    let repo = client.repo(*repo_id).await?;
                    let branch = repo.load_branch(branch_name).await?;
                    branch.create_document();
                }

                TestOperation::EditDocument {
                    client_id,
                    repo_id,
                    branch_name,
                    document_id,
                    edits,
                } => {
                    let client = clients
                        .iter_mut()
                        .find(|c| c.id == *client_id)
                        .ok_or_else(|| anyhow!("client not found"))?;
                    let repo = client.repo(*repo_id).await?;
                    let branch = repo.load_branch(branch_name).await?;
                    let document = branch.load_document(*document_id)?;
                    document.edit(edits.iter().cloned());
                }
            }

            Ok(())
        }
    }

    impl Executor for Arc<Background> {
        fn spawn<F>(&self, future: F)
        where
            F: 'static + Send + Future<Output = ()>,
        {
            Background::spawn(self, future).detach();
        }
    }
}