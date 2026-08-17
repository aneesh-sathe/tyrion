use crate::domain::EvidenceOutcome;
use sha2::{Digest, Sha256};

pub struct VerificationOutcome {
    pub outcome: EvidenceOutcome,
    pub observed: String,
}

pub fn exact_match(expected: &str, observed: &str, artifact_revision: &str) -> VerificationOutcome {
    let observed_artifact_revision = format!("sha256:{:x}", Sha256::digest(observed.as_bytes()));
    VerificationOutcome {
        outcome: if observed == expected && artifact_revision == observed_artifact_revision {
            EvidenceOutcome::Passed
        } else {
            EvidenceOutcome::Failed
        },
        observed: observed.to_owned(),
    }
}
