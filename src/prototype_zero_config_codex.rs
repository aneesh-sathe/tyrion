//! PROTOTYPE ONLY.
//!
//! Question: can `tyrion codex` synthesize session-local Worker inputs and
//! complete one tiny Git Commission without user-authored JSON or persistent
//! harness configuration, assuming Tyrion already owns a vetted contained
//! runtime bundle?

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Phase {
    Inspecting,
    RuntimeReady,
    EntryAttached,
    CommissionAccepted,
    Verified,
}

pub struct RuntimeFacts {
    pub tyrion_owned_bundle: bool,
    pub boundary_attested: bool,
    pub docker_version: Option<String>,
    pub guest_codex_version: Option<String>,
    pub ambient_codex_version: Option<String>,
}

pub enum RuntimeDecision {
    Ready { boundary_attested: bool },
    Blocked { reasons: Vec<String> },
}

impl RuntimeDecision {
    pub fn summary(&self) -> String {
        match self {
            Self::Ready {
                boundary_attested: true,
            } => "ready: pinned Tyrion-owned runtime with attested boundary".into(),
            Self::Ready {
                boundary_attested: false,
            } => "ready for orchestration only: Tyrion-owned fixture bundle".into(),
            Self::Blocked { reasons } => format!("blocked: {}", reasons.join("; ")),
        }
    }
}

pub fn select_runtime(facts: RuntimeFacts) -> RuntimeDecision {
    let mut reasons = Vec::new();
    if !facts.tyrion_owned_bundle {
        reasons.push("pinned Tyrion-owned runtime bundle missing".into());
    }
    match facts.docker_version.as_deref() {
        Some(version) if version.starts_with("Docker version ") => {}
        Some(version) => reasons.push(format!("expected a Docker CLI, found {version}")),
        None => reasons.push("Docker missing".into()),
    }
    match facts.guest_codex_version.as_deref() {
        Some("codex-cli 0.156.1") => {}
        Some(version) => reasons.push(format!("expected guest Codex 0.156.1, found {version}")),
        None => reasons.push("pinned Linux guest Codex 0.156.1 missing".into()),
    }
    if facts.guest_codex_version.is_none() {
        if let Some(version) = facts.ambient_codex_version {
            reasons.push(format!("ambient host {version} is not a Worker artifact"));
        }
    }
    if reasons.is_empty() {
        RuntimeDecision::Ready {
            boundary_attested: facts.boundary_attested,
        }
    } else {
        RuntimeDecision::Blocked { reasons }
    }
}

impl Phase {
    pub const fn label(&self) -> &'static str {
        match self {
            Self::Inspecting => "inspecting",
            Self::RuntimeReady => "runtime_ready",
            Self::EntryAttached => "entry_attached",
            Self::CommissionAccepted => "commission_accepted",
            Self::Verified => "verified",
        }
    }
}

#[derive(Clone, Debug)]
pub struct PrototypeState {
    pub phase: Phase,
    pub command: &'static str,
    pub user_json: bool,
    pub persistent_harness_config: bool,
    pub principal_checkout_mutated: Option<bool>,
    pub commission_id: Option<String>,
    pub commission_status: Option<String>,
    pub integration_revision: Option<String>,
    pub fixture_sandboxes_created: usize,
    pub fixture_sandboxes_deleted: usize,
    pub production_runtime: String,
}

pub enum Action {
    RuntimePrepared {
        production_runtime: String,
    },
    EntryAttached,
    CommissionAccepted {
        commission_id: String,
    },
    CommissionVerified {
        status: String,
        integration_revision: String,
        principal_checkout_mutated: bool,
        sandboxes_created: usize,
        sandboxes_deleted: usize,
    },
}

impl PrototypeState {
    pub fn new() -> Self {
        Self {
            phase: Phase::Inspecting,
            command: "tyrion codex",
            user_json: false,
            persistent_harness_config: false,
            principal_checkout_mutated: None,
            commission_id: None,
            commission_status: None,
            integration_revision: None,
            fixture_sandboxes_created: 0,
            fixture_sandboxes_deleted: 0,
            production_runtime: "not inspected".into(),
        }
    }

    pub fn apply(&mut self, action: Action) -> Result<(), String> {
        match (&self.phase, action) {
            (Phase::Inspecting, Action::RuntimePrepared { production_runtime }) => {
                self.production_runtime = production_runtime;
                self.phase = Phase::RuntimeReady;
            }
            (Phase::RuntimeReady, Action::EntryAttached) => {
                self.phase = Phase::EntryAttached;
            }
            (Phase::EntryAttached, Action::CommissionAccepted { commission_id }) => {
                self.commission_id = Some(commission_id);
                self.commission_status = Some("accepted".into());
                self.phase = Phase::CommissionAccepted;
            }
            (
                Phase::CommissionAccepted,
                Action::CommissionVerified {
                    status,
                    integration_revision,
                    principal_checkout_mutated,
                    sandboxes_created,
                    sandboxes_deleted,
                },
            ) => {
                self.commission_status = Some(status);
                self.integration_revision = Some(integration_revision);
                self.principal_checkout_mutated = Some(principal_checkout_mutated);
                self.fixture_sandboxes_created = sandboxes_created;
                self.fixture_sandboxes_deleted = sandboxes_deleted;
                self.phase = Phase::Verified;
            }
            (phase, _) => {
                return Err(format!("action is not valid while {}", phase.label()));
            }
        }
        Ok(())
    }

    pub fn render(&self) -> String {
        format!(
            concat!(
                "\x1b[1mZero-config Codex Commission prototype\x1b[0m\n",
                "\x1b[2mFixture-backed orchestration proof, not MicroVM attestation\x1b[0m\n\n",
                "\x1b[1mphase\x1b[0m: {}\n",
                "\x1b[1mcommand\x1b[0m: {}\n",
                "\x1b[1muser-authored JSON\x1b[0m: {}\n",
                "\x1b[1mpersistent harness config\x1b[0m: {}\n",
                "\x1b[1mcommission\x1b[0m: {}\n",
                "\x1b[1mstatus\x1b[0m: {}\n",
                "\x1b[1mintegration revision\x1b[0m: {}\n",
                "\x1b[1mprincipal checkout mutated\x1b[0m: {}\n",
                "\x1b[1mfixture sandboxes\x1b[0m: {} created, {} deleted\n",
                "\x1b[1mproduction runtime\x1b[0m: {}\n\n",
                "\x1b[2m[q] quit is automatic after the verdict\x1b[0m\n"
            ),
            self.phase.label(),
            self.command,
            self.user_json,
            self.persistent_harness_config,
            self.commission_id.as_deref().unwrap_or("pending"),
            self.commission_status.as_deref().unwrap_or("pending"),
            self.integration_revision.as_deref().unwrap_or("pending"),
            self.principal_checkout_mutated
                .map(|mutated| mutated.to_string())
                .unwrap_or_else(|| "pending".into()),
            self.fixture_sandboxes_created,
            self.fixture_sandboxes_deleted,
            self.production_runtime,
        )
    }
}
