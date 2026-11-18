use aris_tools::{ToolContext, ToolDefinition, ToolError, ToolHandler, ToolMetadata};
use async_trait::async_trait;
use serde::Deserialize;
use serde_json::{json, Value};

pub(crate) struct ReportCalculate;

#[derive(Deserialize)]
struct Check {
    label: String,
    terms: Vec<Vec<f64>>,
    #[serde(default = "one")]
    divisor: f64,
}

fn one() -> f64 {
    1.0
}

fn calculate(check: Check) -> Result<Value, ToolError> {
    if check.label.trim().is_empty()
        || check.label.len() > 240
        || check.terms.is_empty()
        || check.terms.len() > 64
        || check.terms.iter().any(|t| t.is_empty() || t.len() > 16)
        || check.divisor == 0.0
    {
        return Err(ToolError::Message(
            "Each check needs a label, 1–64 terms of 1–16 factors, and a nonzero divisor".into(),
        ));
    }
    let sum = check
        .terms
        .iter()
        .map(|term| term.iter().product::<f64>())
        .sum::<f64>();
    let result = sum / check.divisor;
    if !result.is_finite() {
        return Err(ToolError::Message(
            "Calculation exceeded finite numeric range".into(),
        ));
    }
    Ok(json!({"label":check.label,"result":result,"divisor":check.divisor,"terms":check.terms}))
}

#[async_trait]
impl ToolHandler for ReportCalculate {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition::builtin("report_calculate",
            "Verify report scores, budgets and ratios from original inputs in a single batch. Each result is sum(product(term)) / divisor; use negative factors to subtract. Example: 4×30% + 3×70% uses terms [[4,0.3],[3,0.7]].",
            json!({"type":"object","properties":{"checks":{"type":"array","minItems":1,"maxItems":16,"items":{
                "type":"object","properties":{"label":{"type":"string"},"terms":{"type":"array","items":{"type":"array","items":{"type":"number"}}},"divisor":{"type":"number","description":"Defaults to 1"}},
                "required":["label","terms"]}}},"required":["checks"]})
        ).with_metadata(ToolMetadata::read_only())
    }

    async fn call(&self, args: Value, _ctx: &ToolContext) -> Result<String, ToolError> {
        #[derive(Deserialize)]
        struct Batch {
            checks: Vec<Check>,
        }
        let batch: Batch =
            serde_json::from_value(args).map_err(|e| ToolError::Message(e.to_string()))?;
        if !(1..=16).contains(&batch.checks.len()) {
            return Err(ToolError::Message("Supply 1–16 checks".into()));
        }
        let rows = batch
            .checks
            .into_iter()
            .map(calculate)
            .collect::<Result<Vec<_>, _>>()?;
        Ok(json!(rows).to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn verifies_original_score_and_rejects_undefined_conversions() {
        let check: Check = serde_json::from_value(json!({
            "label":"priority","terms":[[4,0.30],[4,0.25],[4,0.20],[3,0.15],[3,0.10]]
        }))
        .unwrap();
        assert!((calculate(check).unwrap()["result"].as_f64().unwrap() - 3.75).abs() < 1e-12);
        let no_paying_customers: Check =
            serde_json::from_value(json!({"label":"CAC","terms":[[4000]],"divisor":0})).unwrap();
        assert!(calculate(no_paying_customers).is_err());
    }
}
