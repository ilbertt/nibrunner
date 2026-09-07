pub mod control_plane_controller;
pub mod converge_controller;
pub mod filesystem_controller;
pub mod lifecycle_controller;
pub mod measurement_controller;
pub mod status_controller;

use async_trait::async_trait;

#[async_trait]
pub trait Controller: Send + Sync {
    fn name(&self) -> &'static str;
    async fn run(&self);
}
