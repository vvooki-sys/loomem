//! JSONL contract between the trusted Hosted owner and one shared engine.
use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::Value;

#[derive(Clone, Debug, Deserialize, Eq, Hash, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ScopeContext {
    pub actor: String,
    pub scope: String,
    pub epoch: i64,
}

impl ScopeContext {
    pub fn validate(&self) -> Result<()> {
        if !safe_name(&self.actor) || !safe_name(&self.scope) || self.epoch < 0 {
            bail!("invalid actor, scope or epoch")
        }
        Ok(())
    }
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HostedRecord {
    pub op_id: String,
    pub id: String,
    pub content: String,
    pub embedding: Vec<f32>,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Request {
    pub cmd: String,
    #[serde(default)]
    pub context: Option<ScopeContext>,
    #[serde(default)]
    pub request_id: Option<String>,
    #[serde(default)]
    pub generation: Option<String>,
    #[serde(default)]
    pub op_id: Option<String>,
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub args: Option<Value>,
    #[serde(default)]
    pub checkpoint: Option<String>,
    #[serde(default)]
    pub root: Option<String>,
    #[serde(default)]
    pub content: Option<String>,
    #[serde(default)]
    pub records: Option<Vec<HostedRecord>>,
    #[serde(default)]
    pub failpoint: Option<String>,
    #[serde(default)]
    pub query: Option<String>,
    #[serde(default)]
    pub embedding: Option<Vec<f32>>,
    #[serde(default)]
    pub id: Option<String>,
}

impl Request {
    pub fn context(&self) -> Result<ScopeContext> {
        let context = self.context.clone().context("missing scope context")?;
        context.validate()?;
        Ok(context)
    }

    pub fn operation(&self) -> Result<String> {
        let operation = self.op_id.clone().context("missing op_id")?;
        if !safe_name(&operation) {
            bail!("invalid op_id")
        }
        Ok(operation)
    }

    pub fn generation(&self) -> Result<String> {
        let generation = self.generation.clone().context("missing generation")?;
        if !safe_name(&generation) {
            bail!("invalid generation")
        }
        Ok(generation)
    }

    pub fn name(&self) -> Result<&str> {
        self.name.as_deref().context("missing tool or worker name")
    }
}

pub fn safe_name(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 128
        && value
            .bytes()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, b'_' | b'-'))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn context_requires_explicit_safe_identity() {
        let request: Request = serde_json::from_value(serde_json::json!({"cmd":"tool_async"}))
            .expect("deserialize request");
        assert!(request.context().is_err());
        let context = ScopeContext {
            actor: "actor:a".into(),
            scope: "scope".into(),
            epoch: 1,
        };
        assert!(context.validate().is_err());
    }
}
