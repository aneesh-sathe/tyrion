//! Tyrion-owned planning. A Principal who does not want to write a plan asks
//! for one: a contained, read-only planning Worker reads the repository and
//! proposes the Assignments, and the Control Plane validates them against the
//! accepted mandate before anything that writes is dispatched.
//!
//! The planner proposes; it never grants. It cannot widen authority, and it
//! does not choose resources: those follow from the mandate.

use std::collections::{HashMap, HashSet};

use serde::Deserialize;

use crate::protocol::{
    AssignmentPurpose, AssignmentResources, CommissionPlan, PlannedAssignment, ResourceCeilings,
    WorkerRequirements,
};

/// Planning Assignments are named `tyrion-plan-<n>`; proposed Assignments may
/// not use the `tyrion-` prefix.
pub(super) const PLANNING_PREFIX: &str = "tyrion-plan-";
/// One planning Attempt, and one more with the rejection as feedback.
pub(super) const MAX_PLANNING_ROUNDS: u32 = 2;
/// Planning returns text, not code, so its Result needs little room.
const PLANNING_STORAGE_BYTES: u64 = 5 * 1024 * 1024;

pub(super) fn is_planning(logical_id: &str) -> bool {
    logical_id.starts_with(PLANNING_PREFIX)
}

pub(super) struct CriterionBrief {
    pub id: String,
    pub description: String,
    pub check: String,
}

pub(super) struct PlanningBrief<'a> {
    pub goal: &'a str,
    pub constraints: &'a [String],
    pub criteria: &'a [CriterionBrief],
    pub authorized_paths: &'a [String],
    pub max_worker_concurrency: u32,
    /// Why the previous proposal was rejected, when this is a second round.
    pub rejection: Option<&'a str>,
}

/// The planning Worker's whole instruction. It sees only this and the
/// repository, so everything it needs to plan is here.
pub(super) fn planning_goal(brief: &PlanningBrief<'_>) -> String {
    let mut goal = String::from(
        "You are the planning Worker for a Tyrion Commission. Do not change any files. \
         Read the repository, then decide how to split the work below into Assignments that \
         other Workers will carry out in parallel.\n\n",
    );
    goal.push_str(&format!("The Principal's goal:\n{}\n\n", brief.goal));
    if !brief.constraints.is_empty() {
        goal.push_str("Constraints every Assignment must respect:\n");
        for constraint in brief.constraints {
            goal.push_str(&format!("- {constraint}\n"));
        }
        goal.push('\n');
    }
    goal.push_str("Acceptance Criteria. Each must be owned by exactly one Assignment:\n");
    for criterion in brief.criteria {
        goal.push_str(&format!(
            "- {}: {} (checked by: {})\n",
            criterion.id, criterion.description, criterion.check
        ));
    }
    goal.push_str("\nPaths Assignments may change (write_scopes must stay inside these):\n");
    for path in brief.authorized_paths {
        goal.push_str(&format!("- {path}\n"));
    }
    goal.push_str(&format!(
        "\nRules:\n\
         - Split only where the parts are genuinely independent. Useful parallelism is the aim, \
           not a large number of Workers; at most {} can run at once. One Assignment is right \
           when the work cannot be split.\n\
         - Each Assignment's goal must be complete instructions for a Worker that sees only that \
           goal and the repository, including which files to create or change and how to check them.\n\
         - Assignments whose write_scopes overlap must be ordered: one lists the other in \
           dependencies.\n\
         - dependencies name other Assignments' ids. Use them only when an Assignment needs \
           another's result.\n\
         - Ids are short lowercase words and may not start with \"tyrion-\".\n\n\
         Put only this JSON in your Result summary, with no other text:\n\
         {{\"assignments\": [{{\"id\": \"...\", \"goal\": \"...\", \"criterion_ids\": [\"...\"], \
         \"write_scopes\": [\"...\"], \"read_scopes\": [], \"dependencies\": []}}]}}\n",
        brief.max_worker_concurrency
    ));
    if let Some(rejection) = brief.rejection {
        goal.push_str(&format!(
            "\nYour previous plan was rejected: {rejection}\nFix exactly that and propose the plan again.\n"
        ));
    }
    goal
}

/// One Assignment as a planner proposes it. Unknown fields are ignored: a
/// model that adds a purpose or resources has not made the plan wrong, and
/// Tyrion sets those itself.
#[derive(Deserialize)]
pub(super) struct ProposedAssignment {
    id: String,
    goal: String,
    criterion_ids: Vec<String>,
    #[serde(default)]
    write_scopes: Vec<String>,
    #[serde(default)]
    read_scopes: Vec<String>,
    #[serde(default)]
    dependencies: Vec<String>,
}

#[derive(Deserialize)]
struct ProposedPlan {
    assignments: Vec<ProposedAssignment>,
}

/// Find the plan in a Worker's summary. Models wrap JSON in prose or code
/// fences often enough that insisting on bare JSON only wastes a round.
pub(super) fn parse_proposed_plan(output: &str) -> Result<Vec<ProposedAssignment>, String> {
    let start = output
        .find('{')
        .ok_or("the summary contains no JSON object")?;
    let end = output
        .rfind('}')
        .ok_or("the summary contains no complete JSON object")?;
    let plan: ProposedPlan = serde_json::from_str(&output[start..=end])
        .map_err(|error| format!("the plan is not valid JSON of the required shape: {error}"))?;
    if plan.assignments.is_empty() {
        return Err("the plan has no Assignments".into());
    }
    if let Some(reserved) = plan
        .assignments
        .iter()
        .find(|assignment| assignment.id.starts_with("tyrion-"))
    {
        return Err(format!(
            "Assignment id {} uses the reserved \"tyrion-\" prefix",
            reserved.id
        ));
    }
    Ok(plan.assignments)
}

/// Turn a proposal into a full plan. Resources come from the mandate, split
/// evenly, never from the planner.
pub(super) fn complete_plan(
    proposed: Vec<ProposedAssignment>,
    ceilings: &ResourceCeilings,
) -> CommissionPlan {
    let count = proposed.len() as u64;
    let storage = (ceilings.max_storage_bytes / count.max(1)).max(1);
    CommissionPlan {
        assignments: proposed
            .into_iter()
            .map(|assignment| PlannedAssignment {
                id: assignment.id,
                goal: assignment.goal,
                dependencies: assignment.dependencies,
                criterion_ids: assignment.criterion_ids,
                purpose: AssignmentPurpose::CriticalPath,
                read_scopes: assignment.read_scopes,
                write_scopes: assignment.write_scopes,
                resources: AssignmentResources {
                    concurrency_slots: 1,
                    max_storage_bytes: storage,
                    max_model_spend_cents: 0,
                    max_paid_service_spend_cents: 0,
                },
                worker_requirements: WorkerRequirements::default(),
                competition: None,
            })
            .collect(),
    }
}

/// The planning step itself: one read-only Assignment that owns no criterion.
pub(super) fn planning_plan(
    round: u32,
    goal: String,
    ceilings: &ResourceCeilings,
    worker_requirements: WorkerRequirements,
) -> CommissionPlan {
    CommissionPlan {
        assignments: vec![PlannedAssignment {
            id: format!("{PLANNING_PREFIX}{round}"),
            goal,
            dependencies: Vec::new(),
            criterion_ids: Vec::new(),
            purpose: AssignmentPurpose::UncertaintyReduction,
            read_scopes: Vec::new(),
            write_scopes: Vec::new(),
            resources: AssignmentResources {
                concurrency_slots: 1,
                max_storage_bytes: PLANNING_STORAGE_BYTES.min(ceilings.max_storage_bytes),
                max_model_spend_cents: 0,
                max_paid_service_spend_cents: 0,
            },
            worker_requirements,
            competition: None,
        }],
    }
}

/// A Principal's plan may leave overlapping writers unordered and let the
/// frontier serialize them. A planner's may not: two Workers told to change
/// the same file at once is a planning mistake, and it is cheaper to ask again
/// than to reconcile afterwards.
pub(super) fn ensure_overlaps_are_ordered(plan: &CommissionPlan) -> Result<(), String> {
    let dependencies: HashMap<&str, Vec<&str>> = plan
        .assignments
        .iter()
        .map(|assignment| {
            (
                assignment.id.as_str(),
                assignment.dependencies.iter().map(String::as_str).collect(),
            )
        })
        .collect();
    let depends_on = |from: &str, target: &str| {
        let mut seen = HashSet::new();
        let mut stack = vec![from];
        while let Some(current) = stack.pop() {
            for &next in dependencies.get(current).into_iter().flatten() {
                if next == target {
                    return true;
                }
                if seen.insert(next) {
                    stack.push(next);
                }
            }
        }
        false
    };
    for (index, left) in plan.assignments.iter().enumerate() {
        for right in &plan.assignments[index + 1..] {
            let overlap = left.write_scopes.iter().find(|left_scope| {
                right.write_scopes.iter().any(|right_scope| {
                    super::frontier::scopes_overlap(
                        std::slice::from_ref(*left_scope),
                        std::slice::from_ref(right_scope),
                    )
                })
            });
            if let Some(scope) = overlap {
                if !depends_on(&left.id, &right.id) && !depends_on(&right.id, &left.id) {
                    return Err(format!(
                        "Assignments {} and {} both write {scope} but neither depends on the other; order them or merge them",
                        left.id, right.id
                    ));
                }
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ceilings() -> ResourceCeilings {
        ResourceCeilings {
            max_attempts: 8,
            max_elapsed_seconds: 900,
            max_worker_concurrency: 4,
            max_storage_bytes: 40,
            max_model_spend_cents: 0,
            max_paid_service_spend_cents: 0,
        }
    }

    #[test]
    fn a_plan_is_found_inside_prose_and_code_fences() {
        let output = "Here is the plan:\n```json\n{\"assignments\": [{\"id\": \"a\", \"goal\": \"g\", \"criterion_ids\": [\"c\"], \"write_scopes\": [\"a.py\"], \"purpose\": \"ignored\"}]}\n```\nDone.";
        let plan = complete_plan(parse_proposed_plan(output).unwrap(), &ceilings());
        assert_eq!(plan.assignments[0].id, "a");
        // Resources come from the mandate, never the planner.
        assert_eq!(plan.assignments[0].resources.max_storage_bytes, 40);
        assert!(parse_proposed_plan("no plan here").is_err());
        assert!(parse_proposed_plan("{\"assignments\": []}").is_err());
        let reserved = parse_proposed_plan(
            "{\"assignments\": [{\"id\": \"tyrion-x\", \"goal\": \"g\", \"criterion_ids\": []}]}",
        );
        assert!(reserved.err().unwrap().contains("reserved"));
    }

    #[test]
    fn overlapping_writers_must_be_ordered() {
        let plan = |dependencies: &[&str]| {
            complete_plan(
                parse_proposed_plan(&format!(
                    "{{\"assignments\": [\
                     {{\"id\": \"a\", \"goal\": \"g\", \"criterion_ids\": [\"x\"], \"write_scopes\": [\"src\"]}},\
                     {{\"id\": \"b\", \"goal\": \"g\", \"criterion_ids\": [\"y\"], \"write_scopes\": [\"src/lib.rs\"], \"dependencies\": {}}},\
                     {{\"id\": \"c\", \"goal\": \"g\", \"criterion_ids\": [\"z\"], \"write_scopes\": [\"docs\"]}}]}}",
                    serde_json::to_string(dependencies).unwrap()
                ))
                .unwrap(),
                &ceilings(),
            )
        };
        let unordered = ensure_overlaps_are_ordered(&plan(&[])).unwrap_err();
        assert!(unordered.contains("a and b both write"), "{unordered}");
        ensure_overlaps_are_ordered(&plan(&["a"])).unwrap();
    }

    #[test]
    fn the_planning_goal_carries_everything_the_planner_needs() {
        let criteria = [CriterionBrief {
            id: "fees".into(),
            description: "fees work".into(),
            check: "python -m unittest tests.test_fees".into(),
        }];
        let paths = ["ledger/fees.py".to_owned()];
        let goal = planning_goal(&PlanningBrief {
            goal: "Add fees",
            constraints: &["Keep functions short".to_owned()],
            criteria: &criteria,
            authorized_paths: &paths,
            max_worker_concurrency: 3,
            rejection: Some("Assignment a names unknown criterion q"),
        });
        for expected in [
            "planning Worker for a Tyrion Commission",
            "Add fees",
            "Keep functions short",
            "fees: fees work (checked by: python -m unittest tests.test_fees)",
            "- ledger/fees.py",
            "at most 3 can run at once",
            "previous plan was rejected: Assignment a names unknown criterion q",
        ] {
            assert!(goal.contains(expected), "missing {expected}");
        }
    }
}
