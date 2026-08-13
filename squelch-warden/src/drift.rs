//! What is on a tenant's Deployment that the warden did not put there.
//!
//! Server-side apply owns FIELDS, not objects. Every apply the warden makes
//! declares the fields in [`crate::objects::deployment`] and stamps them with
//! [`crate::cluster::FIELD_MANAGER`]; anything else on the object belongs to
//! whoever wrote it, and an apply neither reports it nor removes it. A single
//! `kubectl set env` against a tenant is therefore invisible to this service
//! forever: the field is real, the pod carries it, and the next twenty applies
//! converge without ever mentioning it.
//!
//! So a drift report is two independent questions, and neither one answers the
//! other:
//!
//! 1. **Who else owns something here?** [`foreign_managers`] reads
//!    `metadata.managedFields`, which is the API server's own record of which
//!    manager owns which field. A field the warden does not declare shows up
//!    here and nowhere else.
//! 2. **What would a re-apply change?** [`diff_spec`] compares the live spec
//!    against the spec the API server says an apply of today's render would
//!    produce. That covers the fields the warden DOES declare and that
//!    something has since moved, and it covers a render that has moved on from
//!    what is running.
//!
//! Both functions are pure: JSON in, report out. The cluster round trip lives
//! in [`crate::provision::Warden::drift`], which is what makes every shape
//! below testable against a fixture rather than against a cluster.
//!
//! PRIVACY: a Deployment spec holds no secret material. Secrets reach a tenant
//! pod as REFERENCES by name, mounted or projected by the kubelet, so a field
//! value quoted in one of these reports is a name, an image, a path or a
//! resource quantity. Nothing here may be pointed at a Secret's contents.

use k8s_openapi::api::apps::v1::Deployment;
use serde::Serialize;
use serde_json::Value;

use crate::cluster::FIELD_MANAGER;

/// A tenant's Deployment, as the warden sees it versus as it would write it.
///
/// Read-only and diagnostic: the answer to "is this tenant still the thing we
/// rendered?", which is a question nothing else in the system can answer.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct DriftReport {
    /// The tenant's status word, from [`crate::provision::TenantStatus`].
    pub status: &'static str,
    /// Whether a Deployment exists at all. False for a tenant that was never
    /// provisioned and for one that was stopped, and both arrays are empty
    /// there: there is no object to have drifted.
    pub deployment_present: bool,
    /// Field managers other than the warden that own something on this object.
    pub foreign: Vec<ForeignManager>,
    /// What an apply of today's render would change.
    pub changes: Vec<FieldChange>,
}

/// One other writer, and what it owns.
///
/// The presence of an entry is the finding. Any surviving path means somebody
/// took ownership of part of a tenant's workload, which is exactly the state
/// that detonates later: the warden converges around it and the field stays
/// until something unrelated makes it fatal.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct ForeignManager {
    /// The `fieldManager` string, as the API server recorded it (`kubectl-set`,
    /// `kubectl-edit`, a controller's name).
    pub manager: String,
    /// `Apply` or `Update`.
    pub operation: String,
    /// Dotted paths, sorted. See [`foreign_managers`] for the spelling.
    pub paths: Vec<String>,
}

/// One field that an apply would move, with both sides of it.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct FieldChange {
    /// Dotted, rooted at `spec`, with keyed list items in brackets:
    /// `spec.template.spec.containers[squelchd].image`.
    pub path: String,
    /// What the object carries now. `null` when the field only exists in the
    /// render.
    pub live: Value,
    /// What an apply would leave. `null` when the field only exists live.
    pub rendered: Value,
}

/// The fieldsV1 key that marks the enclosing path as owned rather than naming
/// a child of it.
const SELF_MARKER: &str = ".";

/// The deployment controller's bookkeeping, and the one path a report drops.
///
/// It is written on every rollout by design and it is not on any object the
/// warden declares, so reporting it would mean every tenant is permanently
/// drifted and the report is worth nothing.
const REVISION_PATH: &str = "metadata.annotations.deployment.kubernetes.io/revision";

/// The roots a report keeps.
///
/// `status` is the controller's to write and `metadata` is mostly the API
/// server's; what a person can break by hand is the spec, the labels this
/// warden selects on, and the annotations that decide when a pod rolls.
const REPORTED_ROOTS: [&str; 3] = ["spec", "metadata.labels", "metadata.annotations"];

/// Every field manager other than the warden that owns part of this object.
///
/// `metadata.managedFields` is the API server's ownership ledger: one entry per
/// (manager, operation, subresource), each carrying a fieldsV1 trie of what it
/// owns. This walks each entry into dotted paths and keeps the entry when
/// anything survives the filter.
///
/// The path spelling, which is the one thing a reader has to trust:
///
/// - `f:<name>` is a struct field or map key, joined with a dot;
/// - `k:{...}` is a keyed list item, joined as `[name]` when the key is a name
///   alone (every list in a pod spec that matters here) and as `[{...}]`
///   otherwise;
/// - `v:<json>` is a set item, joined as `[=<json>]`;
/// - `.` marks the enclosing path itself and adds nothing.
///
/// Two entries are ignored outright. The warden's own is not drift by
/// definition, and a `status` subresource entry is the deployment controller
/// reporting on the object rather than changing it.
///
/// An entry that owns a structural parent and no leaf still counts: a
/// `set env` that was reverted with a second `set env` leaves the container's
/// `env` owned by a foreign manager with nothing under it, and that tombstone
/// is precisely what makes the next hand edit or the next controller land
/// somewhere the warden cannot see.
pub fn foreign_managers(deployment: &Deployment) -> Vec<ForeignManager> {
    let Some(entries) = deployment.metadata.managed_fields.as_ref() else {
        return Vec::new();
    };
    let mut foreign = Vec::new();
    for entry in entries {
        let manager = entry.manager.clone().unwrap_or_default();
        if manager == FIELD_MANAGER || entry.subresource.as_deref() == Some("status") {
            continue;
        }
        let Some(fields) = entry.fields_v1.as_ref() else {
            continue;
        };
        let mut paths = Vec::new();
        flatten(&fields.0, "", &mut paths);
        paths.retain(|path| reported(path) && path != REVISION_PATH);
        paths.sort();
        paths.dedup();
        if paths.is_empty() {
            continue;
        }
        foreign.push(ForeignManager {
            manager,
            operation: entry.operation.clone().unwrap_or_default(),
            paths,
        });
    }
    foreign
}

/// Whether a path is one this report speaks about. See [`REPORTED_ROOTS`].
fn reported(path: &str) -> bool {
    REPORTED_ROOTS.iter().any(|root| {
        path == *root
            || path
                .strip_prefix(root)
                .is_some_and(|rest| rest.starts_with('.') || rest.starts_with('['))
    })
}

/// Walk a fieldsV1 trie into dotted paths.
///
/// A node with no children but the self-marker IS the path: that is how
/// structured-merge-diff spells a leaf, and how it spells an ownership
/// tombstone over a parent whose children have all gone.
fn flatten(node: &Value, path: &str, out: &mut Vec<String>) {
    let Some(children) = node.as_object() else {
        // Not a trie. The API server does not write this, and inventing a
        // child path for it would be inventing a finding.
        if !path.is_empty() {
            out.push(path.to_string());
        }
        return;
    };
    let mut leaf = true;
    for (key, child) in children {
        if key == SELF_MARKER {
            continue;
        }
        leaf = false;
        flatten(child, &join(path, key), out);
    }
    if leaf && !path.is_empty() {
        out.push(path.to_string());
    }
}

/// Append one fieldsV1 key to a path, in the spelling [`foreign_managers`]
/// documents.
fn join(path: &str, key: &str) -> String {
    let dotted = |segment: &str| {
        if path.is_empty() {
            segment.to_string()
        } else {
            format!("{path}.{segment}")
        }
    };
    if let Some(field) = key.strip_prefix("f:") {
        dotted(field)
    } else if let Some(keys) = key.strip_prefix("k:") {
        format!("{path}[{}]", list_key(keys))
    } else if let Some(item) = key.strip_prefix("v:") {
        format!("{path}[={item}]")
    } else if let Some(index) = key.strip_prefix("i:") {
        format!("{path}[{index}]")
    } else {
        // An unprefixed key is a shape structured-merge-diff does not define.
        // Carried through verbatim: a path nobody can read still names the
        // manager that owns it, which is the finding.
        dotted(key)
    }
}

/// The bracket for a `k:` list key.
///
/// Keyed lists in a pod spec are keyed by `name` alone - containers, env,
/// volumes, mounts, ports on a Service - so the common answer is the name
/// itself. Anything else is re-serialized compactly, and text that is not JSON
/// at all is kept as it arrived rather than dropped.
fn list_key(keys: &str) -> String {
    let Ok(parsed) = serde_json::from_str::<Value>(keys) else {
        return keys.to_string();
    };
    if let Some(map) = parsed.as_object()
        && map.len() == 1
        && let Some(name) = map.get("name").and_then(Value::as_str)
    {
        return name.to_string();
    }
    serde_json::to_string(&parsed).unwrap_or_else(|_| keys.to_string())
}

/// What an apply of the rendered spec would change about the live one.
///
/// Both arguments are a Deployment's `.spec`, serialized. Paths come back
/// rooted at `spec` so they read the same way as the ones in
/// [`ForeignManager::paths`], which is what lets a person line the two halves
/// of a report up by eye.
///
/// Lists whose every element carries a string `name` are aligned by that name
/// rather than by index, because that is how the API server merges them: a
/// container list that gained an entry has not changed every container after
/// it, and a report that said so would bury the one field that moved.
/// Everything else is compared whole, since an index-keyed list has no stable
/// identity to diff against.
pub fn diff_spec(live: &Value, rendered: &Value) -> Vec<FieldChange> {
    let mut changes = Vec::new();
    diff(SPEC_ROOT, live, rendered, &mut changes);
    changes
}

/// The path both halves of a report are rooted at.
const SPEC_ROOT: &str = "spec";

fn diff(path: &str, live: &Value, rendered: &Value, out: &mut Vec<FieldChange>) {
    match (live, rendered) {
        (Value::Object(l), Value::Object(r)) => {
            let mut keys: Vec<&String> = l.keys().chain(r.keys()).collect();
            keys.sort();
            keys.dedup();
            for key in keys {
                let child = format!("{path}.{key}");
                match (l.get(key), r.get(key)) {
                    (Some(live), Some(rendered)) => diff(&child, live, rendered, out),
                    (live, rendered) => out.push(change(child, live, rendered)),
                }
            }
        }
        (Value::Array(l), Value::Array(r)) if named(l) && named(r) => {
            for name in names(l, r) {
                let child = format!("{path}[{name}]");
                let live = l.iter().find(|item| name_of(item) == Some(name.as_str()));
                let rendered = r.iter().find(|item| name_of(item) == Some(name.as_str()));
                match (live, rendered) {
                    (Some(live), Some(rendered)) => diff(&child, live, rendered, out),
                    (live, rendered) => out.push(change(child, live, rendered)),
                }
            }
        }
        _ if live != rendered => out.push(change(path.to_string(), Some(live), Some(rendered))),
        _ => {}
    }
}

/// One change, with an absent side spelled `null`.
fn change(path: String, live: Option<&Value>, rendered: Option<&Value>) -> FieldChange {
    FieldChange {
        path,
        live: live.cloned().unwrap_or(Value::Null),
        rendered: rendered.cloned().unwrap_or(Value::Null),
    }
}

/// Whether a list can be aligned by name. Vacuously true for an empty list,
/// which is what makes "the render added the first container" a per-name
/// change rather than a whole-list one.
fn named(items: &[Value]) -> bool {
    items.iter().all(|item| name_of(item).is_some())
}

/// Every name in either list, live order first so a report reads in the order
/// the object does.
fn names(live: &[Value], rendered: &[Value]) -> Vec<String> {
    let mut names: Vec<String> = Vec::new();
    for name in live.iter().chain(rendered).filter_map(name_of) {
        if !names.iter().any(|seen| seen == name) {
            names.push(name.to_string());
        }
    }
    names
}

fn name_of(item: &Value) -> Option<&str> {
    item.get("name")?.as_str()
}

#[cfg(test)]
mod tests {
    use super::*;
    use k8s_openapi::apimachinery::pkg::apis::meta::v1::{FieldsV1, ManagedFieldsEntry, ObjectMeta};
    use serde_json::json;

    /// One managedFields entry, the way the API server writes them.
    fn entry(manager: &str, operation: &str, fields: Value) -> ManagedFieldsEntry {
        ManagedFieldsEntry {
            manager: Some(manager.to_string()),
            operation: Some(operation.to_string()),
            fields_type: Some("FieldsV1".to_string()),
            fields_v1: Some(FieldsV1(fields)),
            ..Default::default()
        }
    }

    fn managed(entries: Vec<ManagedFieldsEntry>) -> Deployment {
        Deployment {
            metadata: ObjectMeta {
                managed_fields: Some(entries),
                ..Default::default()
            },
            ..Default::default()
        }
    }

    /// What `kubectl set env deployment/<label> ... --containers=seed` leaves
    /// behind: a foreign manager owning an env entry, complete with the secret
    /// reference that goes fatal the day the Secret is deleted.
    fn set_env_fields() -> Value {
        json!({
            "f:spec": {
                "f:template": {
                    "f:spec": {
                        "f:initContainers": {
                            "k:{\"name\":\"seed\"}": {
                                "f:env": {
                                    "k:{\"name\":\"SQUELCH_ANTHROPIC_API_KEY\"}": {
                                        ".": {},
                                        "f:name": {},
                                        "f:valueFrom": {
                                            "f:secretKeyRef": {
                                                ".": {},
                                                "f:key": {},
                                                "f:name": {}
                                            }
                                        }
                                    }
                                }
                            }
                        }
                    }
                }
            }
        })
    }

    /// The incident, as a fixture. A hand edit owns a field the warden never
    /// declares, so no number of applies mentions it and this scan is the only
    /// thing that can.
    #[test]
    fn a_hand_edited_env_var_is_reported_against_its_manager() {
        let deployment = managed(vec![entry("kubectl-set", "Update", set_env_fields())]);
        let foreign = foreign_managers(&deployment);
        assert_eq!(foreign.len(), 1);
        assert_eq!(foreign[0].manager, "kubectl-set");
        assert_eq!(foreign[0].operation, "Update");
        assert_eq!(
            foreign[0].paths,
            vec![
                "spec.template.spec.initContainers[seed].env[SQUELCH_ANTHROPIC_API_KEY].name",
                "spec.template.spec.initContainers[seed].env[SQUELCH_ANTHROPIC_API_KEY].valueFrom.secretKeyRef.key",
                "spec.template.spec.initContainers[seed].env[SQUELCH_ANTHROPIC_API_KEY].valueFrom.secretKeyRef.name",
            ]
        );
    }

    /// The fixture the issue calls ready-made: an override that was reverted
    /// leaves the parent owned with nothing under it. The pod looks right and
    /// the ownership is still somebody else's.
    #[test]
    fn a_parent_owned_with_no_leaves_under_it_is_still_drift() {
        let tombstone = json!({
            "f:spec": {
                "f:template": {
                    "f:spec": {
                        "f:containers": {
                            "k:{\"name\":\"squelchd\"}": { "f:env": {} }
                        }
                    }
                }
            }
        });
        let deployment = managed(vec![entry("kubectl-client-side-apply", "Update", tombstone)]);
        let foreign = foreign_managers(&deployment);
        assert_eq!(foreign.len(), 1);
        assert_eq!(
            foreign[0].paths,
            vec!["spec.template.spec.containers[squelchd].env"]
        );
    }

    /// Everything a healthy tenant's ledger carries. The warden's own entry,
    /// the controller's status entry, and the revision annotation are the
    /// three things every Deployment has, and a report that flagged any of
    /// them would be a report nobody reads.
    #[test]
    fn the_ordinary_owners_of_a_deployment_are_not_drift() {
        let warden = entry(
            FIELD_MANAGER,
            "Apply",
            json!({
                "f:spec": {
                    "f:replicas": {},
                    "f:template": {
                        "f:spec": {
                            "f:containers": {
                                "k:{\"name\":\"squelchd\"}": { ".": {}, "f:image": {} }
                            }
                        }
                    }
                }
            }),
        );
        let mut status = entry(
            "kube-controller-manager",
            "Update",
            json!({
                "f:status": {
                    "f:availableReplicas": {},
                    "f:conditions": {
                        "k:{\"type\":\"Available\"}": { ".": {}, "f:status": {} }
                    }
                }
            }),
        );
        status.subresource = Some("status".to_string());
        let revision = entry(
            "kube-controller-manager",
            "Update",
            json!({
                "f:metadata": {
                    "f:annotations": {
                        ".": {},
                        "f:deployment.kubernetes.io/revision": {}
                    }
                }
            }),
        );
        // A manager that touched the status without saying so on the
        // subresource: the paths themselves are not ones this report speaks
        // about, so it is still not drift.
        let statusish = entry(
            "some-controller",
            "Update",
            json!({ "f:status": { "f:readyReplicas": {} } }),
        );

        let deployment = managed(vec![warden, status, revision, statusish]);
        assert_eq!(foreign_managers(&deployment), Vec::new());
        // And an object the API server has not written a ledger for at all.
        assert_eq!(foreign_managers(&Deployment::default()), Vec::new());
    }

    /// The whole ledger at once, in the order the API server keeps it: the two
    /// foreign entries come back, the three ordinary ones do not.
    #[test]
    fn a_mixed_ledger_reports_only_the_foreign_entries() {
        let deployment = managed(vec![
            entry(FIELD_MANAGER, "Apply", json!({ "f:spec": { "f:replicas": {} } })),
            entry("kubectl-set", "Update", set_env_fields()),
            entry(
                "kube-controller-manager",
                "Update",
                json!({ "f:metadata": { "f:annotations": { "f:deployment.kubernetes.io/revision": {} } } }),
            ),
            entry(
                "someone",
                "Apply",
                json!({ "f:metadata": { "f:labels": { "f:team": {} } } }),
            ),
        ]);
        let foreign = foreign_managers(&deployment);
        assert_eq!(
            foreign.iter().map(|f| f.manager.as_str()).collect::<Vec<_>>(),
            vec!["kubectl-set", "someone"]
        );
        assert_eq!(foreign[1].paths, vec!["metadata.labels.team"]);
    }

    /// The remaining fieldsV1 spellings, so a path that reaches a person is a
    /// path they can find with `kubectl get -o json`.
    #[test]
    fn every_field_key_shape_becomes_a_readable_path() {
        let deployment = managed(vec![entry(
            "odd",
            "Update",
            json!({
                "f:spec": {
                    // A list keyed by more than a name.
                    "f:ports": { "k:{\"name\":\"http\",\"protocol\":\"TCP\"}": {} },
                    // A set item, and a positional list item.
                    "f:finalizers": { "v:\"keep\"": {} },
                    "f:args": { "i:0": {} },
                    // A key with no prefix at all.
                    "strange": {}
                }
            }),
        )]);
        assert_eq!(
            foreign_managers(&deployment)[0].paths,
            vec![
                "spec.args[0]",
                "spec.finalizers[=\"keep\"]",
                "spec.ports[{\"name\":\"http\",\"protocol\":\"TCP\"}]",
                "spec.strange",
            ]
        );
    }

    #[test]
    fn an_identical_spec_has_nothing_to_change() {
        let spec = json!({
            "replicas": 1,
            "template": {
                "spec": {
                    "containers": [{ "name": "squelchd", "image": "squelchd:v1" }]
                }
            }
        });
        assert_eq!(diff_spec(&spec, &spec), Vec::new());
        assert_eq!(diff_spec(&json!(null), &json!(null)), Vec::new());
    }

    #[test]
    fn a_nested_scalar_is_reported_at_its_own_path() {
        let live = json!({ "strategy": { "type": "Recreate" }, "replicas": 1 });
        let rendered = json!({ "strategy": { "type": "RollingUpdate" }, "replicas": 1 });
        assert_eq!(
            diff_spec(&live, &rendered),
            vec![FieldChange {
                path: "spec.strategy.type".to_string(),
                live: json!("Recreate"),
                rendered: json!("RollingUpdate"),
            }]
        );
    }

    /// Containers and env are name-keyed lists, and the report has to say
    /// which container and which variable rather than "the list differs".
    #[test]
    fn name_keyed_lists_are_aligned_by_name() {
        let live = json!({
            "template": { "spec": { "containers": [
                { "name": "squelchd", "image": "squelchd:v1", "env": [
                    { "name": "SQUELCH_BIND", "value": "0.0.0.0:8848" },
                    { "name": "STRAY", "value": "left-by-hand" }
                ]}
            ]}}
        });
        let rendered = json!({
            "template": { "spec": { "containers": [
                { "name": "squelchd", "image": "squelchd:v2", "env": [
                    { "name": "SQUELCH_BIND", "value": "0.0.0.0:8848" }
                ]}
            ]}}
        });
        assert_eq!(
            diff_spec(&live, &rendered),
            vec![
                FieldChange {
                    path: "spec.template.spec.containers[squelchd].env[STRAY]".to_string(),
                    live: json!({ "name": "STRAY", "value": "left-by-hand" }),
                    rendered: Value::Null,
                },
                FieldChange {
                    path: "spec.template.spec.containers[squelchd].image".to_string(),
                    live: json!("squelchd:v1"),
                    rendered: json!("squelchd:v2"),
                },
            ]
        );
    }

    /// A list element the render adds, and one it drops. Both sides are the
    /// whole element: what a person needs is what would appear or vanish.
    #[test]
    fn a_list_element_on_one_side_only_is_one_change() {
        let live = json!({ "template": { "spec": { "initContainers": [] }}});
        let rendered = json!({ "template": { "spec": { "initContainers": [
            { "name": "seed", "image": "squelchd:v2" }
        ]}}});
        assert_eq!(
            diff_spec(&live, &rendered),
            vec![FieldChange {
                path: "spec.template.spec.initContainers[seed]".to_string(),
                live: Value::Null,
                rendered: json!({ "name": "seed", "image": "squelchd:v2" }),
            }]
        );
        // And the other direction, so a field the render stopped declaring is
        // as visible as one it started declaring.
        assert_eq!(
            diff_spec(&rendered, &live),
            vec![FieldChange {
                path: "spec.template.spec.initContainers[seed]".to_string(),
                live: json!({ "name": "seed", "image": "squelchd:v2" }),
                rendered: Value::Null,
            }]
        );
    }

    #[test]
    fn an_object_key_on_one_side_only_is_one_change_against_null() {
        let live = json!({ "paused": true });
        let rendered = json!({ "replicas": 1 });
        assert_eq!(
            diff_spec(&live, &rendered),
            vec![
                FieldChange {
                    path: "spec.paused".to_string(),
                    live: json!(true),
                    rendered: Value::Null,
                },
                FieldChange {
                    path: "spec.replicas".to_string(),
                    live: Value::Null,
                    rendered: json!(1),
                },
            ]
        );
    }

    /// A list with no name to key on has no stable identity per element, so it
    /// is compared whole rather than pretending index 1 means the same thing
    /// on both sides.
    #[test]
    fn a_list_without_names_is_compared_whole() {
        let live = json!({ "template": { "spec": { "containers": [
            { "name": "squelchd", "args": ["serve", "--verbose"] }
        ]}}});
        let rendered = json!({ "template": { "spec": { "containers": [
            { "name": "squelchd", "args": ["serve"] }
        ]}}});
        assert_eq!(
            diff_spec(&live, &rendered),
            vec![FieldChange {
                path: "spec.template.spec.containers[squelchd].args".to_string(),
                live: json!(["serve", "--verbose"]),
                rendered: json!(["serve"]),
            }]
        );
    }

    /// A field that changed TYPE, which is what a hand edit that replaced a
    /// value with a reference looks like from here.
    #[test]
    fn a_value_that_became_a_reference_is_one_change_carrying_both_shapes() {
        let live = json!({ "env": { "valueFrom": { "secretKeyRef": { "name": "shared" } } } });
        let rendered = json!({ "env": { "value": "inline" } });
        assert_eq!(
            diff_spec(&live, &rendered),
            vec![
                FieldChange {
                    path: "spec.env.value".to_string(),
                    live: Value::Null,
                    rendered: json!("inline"),
                },
                FieldChange {
                    path: "spec.env.valueFrom".to_string(),
                    live: json!({ "secretKeyRef": { "name": "shared" } }),
                    rendered: Value::Null,
                },
            ]
        );
    }
}
