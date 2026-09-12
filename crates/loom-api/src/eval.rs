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
        let actor = match self.store.session(&session)? {
            Some(actor) => actor,
            None => {
                let definition = self
                    .define_authorized(DefineRequest {
                        allowed_effects: Some(Vec::new()),
                        lang: Lang::Rust,
                        name: "loom/session".into(),
                        source: SESSION_SOURCE.into(),
                        deps: BTreeMap::new(),
                    })
                    .await;
                if !definition.ok {
                    return Ok(definition);
                }
                let actor = self
                    .runtime
                    .spawn(
                        definition.result["def"]["hash"]
                            .as_str()
                            .context("session behavior hash missing")?,
                        Value::Null,
                    )
                    .await?;
                self.store.create_session(&session, &actor.id, "owner")?;
                actor.id
            }
        };
        let result = self.runtime.call_def(hash, json!([])).await?;
        self.runtime
            .send(
                &actor,
                json!({"type":"evaluated","source":request.source,"def":hash,"result":result}),
            )
            .await?;
        Ok(self.response(Ok(json!({"session":session,"actor":actor,"value":result}))))
    }
}

const SESSION_SOURCE: &str = r#"
#[loom::actor(effects=[])]
pub struct Session;
impl loom::Actor for Session {
    type State = loom::Value;
    type Event = loom::Value;
    type Msg = loom::Value;
    fn init() -> Self::State { loom::Value::Null }
    fn handle(_: &Self::State, msg: Self::Msg) -> Vec<Self::Event> { vec![msg] }
    fn fold(_: Self::State, event: &Self::Event) -> Self::State { event.clone() }
}
"#;
