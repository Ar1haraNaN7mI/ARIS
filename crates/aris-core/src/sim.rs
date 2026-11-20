//! Optimizer memory. Only versioned, compiled-in policies can change dispatch.
//! Task text, tool arguments and generated reports never become control instructions.
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

pub const HISTORY_LIMIT: usize = 256;
const DISPATCH: &str = "admin.dispatch.native.v1";
const COWORK: &str = "team.cowork.start.v1";

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HarnessProposal {
    pub id: String,
    pub problem: String,
    pub context: String,
    pub edit: String,
    pub rationale: String,
    pub prediction: String,
    pub alternative: String,
    pub evaluation: String,
    pub model: String,
    pub previous: Option<String>,
}

impl HarnessProposal {
    pub fn grade(
        &self,
        passed: bool,
        observation: String,
        measurement_ms: Option<i64>,
    ) -> anyhow::Result<HarnessExperiment> {
        anyhow::ensure!(
            !observation.trim().is_empty() && observation.len() <= 4096,
            "Supply a measured observation of 1–4096 bytes"
        );
        anyhow::ensure!(
            measurement_ms.is_none_or(|ms| ms >= 0),
            "measurement_ms must be nonnegative"
        );
        Ok(HarnessExperiment {
            id: self.id.clone(),
            at: Utc::now(),
            problem: self.problem.clone(),
            edit: self.edit.clone(),
            rationale: self.rationale.clone(),
            prediction: self.prediction.clone(),
            context: self.context.clone(),
            observation,
            measurement_ms,
            passed: Some(passed),
            previous: self.previous.clone(),
        })
    }
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ExternalEvaluation {
    pub evaluation_id: String,
    pub problem: String,
    pub edit: String,
    pub rationale: String,
    pub prediction: String,
    pub context: String,
    pub observation: String,
    pub passed: bool,
    pub measurement_ms: Option<i64>,
}

impl ExternalEvaluation {
    pub fn into_experiment(self) -> anyhow::Result<HarnessExperiment> {
        for id in [&self.evaluation_id, &self.problem] {
            anyhow::ensure!(
                !id.is_empty()
                    && id.len() <= 128
                    && id
                        .bytes()
                        .all(|b| b.is_ascii_alphanumeric() || b"._-".contains(&b)),
                "Use a stable ASCII identifier of 1–128 bytes"
            );
        }
        for text in [
            &self.edit,
            &self.rationale,
            &self.prediction,
            &self.context,
            &self.observation,
        ] {
            anyhow::ensure!(
                !text.trim().is_empty() && text.len() <= 4096,
                "Evaluation fields require 1–4096 bytes"
            );
        }
        anyhow::ensure!(
            self.measurement_ms.is_none_or(|ms| ms >= 0),
            "measurement_ms must be nonnegative"
        );
        Ok(HarnessExperiment {
            id: format!("external/{}/{}", self.problem, self.evaluation_id),
            problem: format!("external.{}", self.problem),
            at: Utc::now(),
            edit: self.edit,
            rationale: self.rationale,
            prediction: self.prediction,
            context: self.context,
            observation: self.observation,
            passed: Some(self.passed),
            measurement_ms: self.measurement_ms,
            previous: None,
        })
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct HarnessExperiment {
    pub id: String,
    pub at: DateTime<Utc>,
    pub problem: String,
    pub edit: String,
    pub rationale: String,
    pub prediction: String,
    pub context: String,
    pub observation: String,
    pub passed: Option<bool>,
    pub measurement_ms: Option<i64>,
    pub previous: Option<String>,
}

#[derive(Clone, Copy, Debug)]
pub enum DispatchOutcome {
    Created,
    Truncated,
    Rejected,
}

#[derive(Clone, Copy, PartialEq)]
enum DispatchEdit {
    RoleContracts,
    SharedContext,
}

impl DispatchEdit {
    fn key(self) -> &'static str {
        match self {
            Self::RoleContracts => "compact-role-contracts",
            Self::SharedContext => "reference-shared-input",
        }
    }
}

pub struct HarnessTrial {
    model: String,
    edit: DispatchEdit,
    previous: Option<String>,
}

impl HarnessTrial {
    pub fn instruction(&self) -> &'static str {
        match self.edit {
            DispatchEdit::RoleContracts => "For a creation call, encode each role as one concise assignment, its owned outputs and direct dependencies. Close the entire native tool object in this turn.",
            DispatchEdit::SharedContext => "For a creation call, refer to the shared goal and input files by name. Put only the unique acceptance criterion in each role prompt and finish every required JSON field.",
        }
    }

    pub(crate) fn propose(history: &[HarnessExperiment], model: &str) -> Self {
        // Compare compatible controls only. The same model gets greater weight,
        // while an unseen model can use evidence from the same tool contract.
        let runs: Vec<_> = history
            .iter()
            .rev()
            .filter(|e| e.problem == DISPATCH && e.id.starts_with("dispatch/"))
            .take(48)
            .collect();
        let score = |edit: DispatchEdit| {
            let mut passed = 1.0;
            let mut total = 2.0;
            for exp in runs.iter().filter(|e| e.edit == edit.key()) {
                let weight = if exp.context == model { 3.0 } else { 1.0 };
                total += weight;
                if exp.passed == Some(true) {
                    passed += weight;
                }
            }
            passed / total
        };
        let edit = if score(DispatchEdit::SharedContext) > score(DispatchEdit::RoleContracts) {
            DispatchEdit::SharedContext
        } else {
            DispatchEdit::RoleContracts
        };
        Self {
            model: model.to_owned(),
            edit,
            previous: runs.first().map(|e| e.id.clone()),
        }
    }

    pub(crate) fn evaluate(
        &self,
        turn: &str,
        attempt: u8,
        outcome: DispatchOutcome,
    ) -> HarnessExperiment {
        let observation = match outcome {
            DispatchOutcome::Created => "Native role creation validated and committed.",
            DispatchOutcome::Truncated => {
                "Creation arguments ended before the full object arrived."
            }
            DispatchOutcome::Rejected => "The control call failed role or schema validation.",
        };
        HarnessExperiment {
            id: format!("dispatch/{turn}/{attempt}"), at: Utc::now(), problem: DISPATCH.into(),
            edit: self.edit.key().into(), rationale: match self.edit {
                DispatchEdit::RoleContracts => "A short contract per role leaves output capacity for the complete roster.",
                DispatchEdit::SharedContext => "Referring to shared inputs avoids repeating the same long goal for every worker.",
            }.into(), prediction: "The complete roster validates and commits atomically.".into(),
            context: self.model.clone(), observation: observation.into(),
            passed: Some(matches!(outcome, DispatchOutcome::Created)), measurement_ms: None, previous: self.previous.clone(),
        }
    }
}

pub(crate) fn cowork_start(message: &str, model: &str, elapsed_ms: i64) -> HarnessExperiment {
    HarnessExperiment {
        id: format!("cowork/{message}"), at: Utc::now(), problem: COWORK.into(),
        edit: "event-driven-ready-lane".into(),
        rationale: "Waiting for a whole batch delayed an eligible collaborator while another lane was still running.".into(),
        prediction: "A delivered invitation reaches an actual recipient run. Latency depends on eligibility and free capacity.".into(),
        context: model.to_owned(), observation: "Recipient started a run associated with the invitation.".into(),
        passed: None, measurement_ms: Some(elapsed_ms), previous: None,
    }
}
