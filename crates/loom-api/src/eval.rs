use super::*;

impl Service {
    pub async fn eval(&self, request: EvalRequest) -> Response {
        if let Err(error) = self.access.require(Scope::Execute) {
            return self.response(Err(error));
        }
        match self.eval_inner(request).await {
            Ok(response) => response,
            Err(error) => self.response(Err(error)),
        }
    }
    async fn eval_inner(&self, request: EvalRequest) -> Result<Response> {
        let session = request
            .session
            .unwrap_or_else(|| uuid::Uuid::new_v4().to_string());
        for alias in request.deps.keys() {
            ensure!(valid_alias(alias), "dependency alias must be an identifier");
        }
        let source = format!(
            "#[loom::def] pub fn main() -> loom::Value {{ loom::serde_json::to_value({{ {} }}).expect(\"eval result must serialize\") }}",
            request.source
        );
        let response = self
            .define_authorized(DefineRequest {
                allowed_effects: None,
                lang: Lang::Rust,
                name: format!("session/{session}/eval"),
                source,
                deps: request.deps,
            })
            .await;
        if !response.ok {
            return Ok(response);
        }
        let hash = response.result["def"]["hash"]
            .as_str()
            .context("definition hash missing")?;
        let result = self.runtime.call_def(hash, json!([])).await?;
        self.store.record_definition_event(&json!({"type":"evaluated","session":session,"source":request.source,"def":hash,"result":result}))?;
        Ok(self.response(Ok(json!({"session":session,"value":result}))))
    }
}
