use crate::LoomMcp;
use loom_actor::Node;
use loom_api::Scope;
use rmcp::{ErrorData, RoleServer, model::*, service::RequestContext};
#[derive(Clone)]
pub struct ActorMcp {
    service: loom_api::actors::ActorService,
}
fn error(error: impl std::fmt::Display) -> ErrorData {
    ErrorData::invalid_params(format!("{error:#}"), None)
}
impl ActorMcp {
    pub fn new(node: Node) -> Self {
        Self {
            service: loom_api::actors::ActorService::new(node),
        }
    }
    pub(crate) async fn resource(&self, uri: &str) -> Result<ReadResourceResult, ErrorData> {
        let value = self.service.resource(uri).await.map_err(error)?;
        Ok(ReadResourceResult {
            contents: vec![ResourceContents::text(value.to_string(), uri)],
        })
    }
}
impl LoomMcp {
    pub(crate) fn actor_access(
        &self,
        context: &RequestContext<RoleServer>,
        scope: Scope,
    ) -> Result<(), ErrorData> {
        context
            .extensions
            .get::<axum::http::request::Parts>()
            .and_then(|parts| parts.extensions.get::<loom_api::Access>())
            .unwrap_or(&self.default_access)
            .require(scope)
            .map_err(error)
    }
}
