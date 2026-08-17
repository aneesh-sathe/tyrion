use sha2::{Digest, Sha256};

pub const CONFIGURATION: &str = "deterministic-local-v1";

pub struct CandidateResult {
    pub output: String,
    pub artifact_revision: String,
}

pub fn execute(goal: &str) -> CandidateResult {
    let output = goal.to_owned();
    let artifact_revision = format!("sha256:{:x}", Sha256::digest(output.as_bytes()));
    CandidateResult {
        output,
        artifact_revision,
    }
}
