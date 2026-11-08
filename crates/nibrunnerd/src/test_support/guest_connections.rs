use super::{app_id, desired_instance, desired_state, instance_record, mocks, test_host, TestHost};
use crate::domain::filesystem::reader::guest_vsock_path;
use std::sync::Arc;
use tokio::net::UnixListener;

pub async fn host() -> (TestHost, tempfile::TempDir, UnixListener) {
    let mut host = test_host().await;
    let directory = tempfile::tempdir_in("/tmp").unwrap();
    let (vms, _) = mocks::vmm_under(directory.path().to_path_buf());
    Arc::get_mut(&mut host.host).unwrap().vms = vms;
    host.cache.lock().await.accept(desired_state(|desired| {
        desired.instances = vec![desired_instance(|_| {})];
    }));
    host.state.put_record(instance_record(|_| {})).await;
    host.state.modify(|snapshot| snapshot.isolated = true).await;
    let path = guest_vsock_path(host.arc(), &app_id());
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    let listener = UnixListener::bind(path).unwrap();
    (host, directory, listener)
}
