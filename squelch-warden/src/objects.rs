//! Every Kubernetes object a tenant is made of, built as typed
//! `k8s-openapi` structs.
//!
//! **The hard rule of this crate lives here: nothing in squelch-warden renders
//! YAML.** There is no template, no YAML serializer, no `format!` that produces a
//! manifest. A tenant label reaches the cluster only as a [`TenantName`] placed
//! in a typed field, which is why [`crate::validate::TenantName`] cannot be
//! constructed without passing the label validator. The static infrastructure
//! that installs the warden itself is YAML, and it lives in `deploy/hosted/`
//! where no tenant value ever appears.
//!
//! A tenant is seven objects in the `tenants` namespace, plus one optional
//! eighth:
//!
//! | Object | Name | Lifetime |
//! |---|---|---|
//! | Secret (identity) | `<label>-identity` | forever; the tenant's age key |
//! | Secret (credential) | `<label>-credential` | forever; the sealed blob |
//! | PersistentVolumeClaim | `<label>-data` | forever; the mailbox index |
//! | Secret (LLM key) | `<label>-llm` | while the control plane keeps one minted |
//! | NetworkPolicy | `<label>` | while provisioned |
//! | Service | `<label>` | while provisioned |
//! | Deployment | `<label>` | while provisioned |
//! | Ingress | `<label>` | while provisioned |
//!
//! DELETE removes the bottom five and keeps the top three, which is what
//! "cancel my account, do not destroy my mail" means: the LLM Secret goes with
//! the workload because it is a gateway credential, not tenant data.

use std::collections::BTreeMap;

use k8s_openapi::api::apps::v1::{Deployment, DeploymentSpec, DeploymentStrategy};
use k8s_openapi::api::core::v1::{
    Capabilities, Container, ContainerPort, EmptyDirVolumeSource, EnvVar, EnvVarSource, KeyToPath,
    LocalObjectReference, PersistentVolumeClaim, PersistentVolumeClaimSpec,
    PersistentVolumeClaimVolumeSource, PodSecurityContext, PodSpec, PodTemplateSpec, Probe,
    ResourceRequirements, SeccompProfile, Secret, SecretKeySelector, SecretVolumeSource,
    SecurityContext, Service, ServicePort, ServiceSpec, TCPSocketAction, Volume, VolumeMount,
    VolumeResourceRequirements,
};
use k8s_openapi::api::networking::v1::{
    HTTPIngressPath, HTTPIngressRuleValue, IPBlock, Ingress, IngressBackend, IngressRule,
    IngressServiceBackend, IngressSpec, IngressTLS, NetworkPolicy, NetworkPolicyEgressRule,
    NetworkPolicyIngressRule, NetworkPolicyPeer, NetworkPolicyPort, NetworkPolicySpec,
    ServiceBackendPort,
};
use k8s_openapi::apimachinery::pkg::api::resource::Quantity;
use k8s_openapi::apimachinery::pkg::apis::meta::v1::{LabelSelector, ObjectMeta};
use k8s_openapi::apimachinery::pkg::util::intstr::IntOrString;

use sha2::{Digest, Sha256};

use crate::config::{Config, Resources};
use crate::identity::TenantIdentity;
use crate::validate::TenantName;

/// The port the daemon binds inside its pod. Every pod has its own network
/// namespace, so this is invisible outside the Service: there is no allocator
/// and no collision, and every tenant uses the same number.
pub const DAEMON_PORT: i32 = 8848;

/// The port the daemon serves Prometheus text metrics on, when
/// `SQUELCH_METRICS_BIND` is set. A listener with no authentication of any
/// kind, which is why it is a second port and not a route on the first.
///
/// **It is deliberately absent from the Service and from the Ingress**, and
/// that absence is the isolation argument: an Ingress can only send traffic to
/// a port its backing Service declares, so no allowlist mistake in
/// [`HUMAN_DOOR_PREFIXES`] can publish this port through a tenant vhost. The
/// only thing that reaches it is the one [`network_policy`] ingress rule that
/// names the monitoring namespace's prometheus-agent pod.
pub const METRICS_PORT: i32 = 9464;

/// The tenant's own volume: SQLite index, the writable credentials file, and
/// the embedding-weights cache under `HOME`.
pub const DATA_DIR: &str = "/data";

/// Where the identity Secret is projected. Read-only, one file.
pub const IDENTITY_DIR: &str = "/etc/squelch/identity";

/// `SQUELCH_CRED_AGE_IDENTITY`.
pub const IDENTITY_FILE: &str = "/etc/squelch/identity/identity.txt";

/// Where the credential Secret is projected, for the init container only. The
/// daemon container never mounts it: it reads the copy on its own volume.
pub const CREDENTIAL_SEED_DIR: &str = "/etc/squelch/credential";

/// `SQUELCH_CREDENTIALS_PATH`. On the tenant's volume, NOT on the Secret mount,
/// because the daemon rewrites this file every time Google hands back a
/// refreshed token and a Secret volume is read-only. The init container seeds
/// it; see [`SEED_SCRIPT`].
pub const CREDENTIAL_PATH: &str = "/data/credentials.json";

/// What the init container remembers about the blob it installed: the SHA-256
/// of the Secret it copied from, and nothing else. Beside the credential on the
/// tenant's own volume, and never readable by anyone the credential is not.
pub const CREDENTIAL_MARKER_PATH: &str = "/data/.credentials.seed";

/// Where the shared, pre-seeded embedding weights are mounted, when the
/// operator configured a model PVC.
pub const MODELS_DIR: &str = "/models";

/// Data key holding the tenant's private age identity.
pub const IDENTITY_KEY: &str = "identity.txt";

/// Data key holding the tenant's public age recipient. Public, but kept beside
/// the identity so the two can never drift apart.
pub const RECIPIENT_KEY: &str = "recipient";

/// Data key holding the tenant's mailbox address.
///
/// In the Secret rather than in the Deployment because Secret data is what k3s
/// encrypts at rest (`--secrets-encryption`); a Deployment's env value is
/// plaintext in the datastore forever. The daemon gets it through a
/// `secretKeyRef`, so the address is never in a pod spec either.
pub const ACCOUNT_EMAIL_KEY: &str = "account-email";

/// Data key holding the age-armored credentials file.
pub const CREDENTIAL_KEY: &str = "credentials.json";

/// Data key holding the tenant's LLM gateway virtual key, in `<label>-llm`.
///
/// The daemon receives it as a `secretKeyRef` (see [`llm_secret`]), and the
/// warden does handle the plaintext: it writes it into the Secret and reads it
/// back to hash it for [`LLM_KEY_HASH_ANNOTATION`]. The posture is narrower:
/// never logged, never returned on the wire, and only the hash lands on the
/// world-readable pod spec.
pub const LLM_API_KEY_KEY: &str = "api-key";

/// Data key holding the tenant's assistant relay virtual key, beside
/// [`LLM_API_KEY_KEY`] in `<label>-llm`.
///
/// The second credential the same Secret can carry: the key the daemon uses to
/// proxy the Passband assistant through the gateway. Present only once the
/// control plane has minted one — a tenant keyed before the assistant era has
/// a Secret without this entry, and its pod boots anyway because the env
/// reference is optional (see [`daemon_env`]). Same posture as the triage key:
/// never logged, never returned on the wire, only its part of the combined
/// hash ([`llm_keys_hash`]) lands on the pod spec.
pub const ASSISTANT_API_KEY_KEY: &str = "assistant-api-key";

/// Data keys in the shared Google OAuth client Secret
/// ([`crate::config::Config::oauth_secret_name`]).
///
/// The warden never reads either one: they go into the pod as `secretKeyRef`,
/// so the client secret is in the API server's encrypted store and in the
/// tenant's process, and never in a manifest or in this process's memory.
pub const OAUTH_CLIENT_ID_KEY: &str = "client_id";
/// See [`OAUTH_CLIENT_ID_KEY`].
pub const OAUTH_CLIENT_SECRET_KEY: &str = "client_secret";

/// Annotation on the pod template carrying the SHA-256 of the credential
/// ciphertext this pod was rolled for.
///
/// It is what makes re-consent possible. A new sealed blob changes only the
/// contents of a Secret, and a Secret's contents are not part of a Deployment's
/// pod template, so nothing would restart and the running daemon would keep
/// using the credential it already has. Putting the hash in the template makes
/// a changed blob a changed pod spec, which is a roll, which re-runs the seed.
/// The HASH, never the ciphertext: an annotation is world-readable to anyone
/// with `get deployment`.
pub const CREDENTIAL_HASH_ANNOTATION: &str = "passband.email/credential-hash";

/// Annotation on the pod template carrying the SHA-256 of the LLM virtual key
/// this pod was rolled for, when one exists.
///
/// Same rationale as [`CREDENTIAL_HASH_ANNOTATION`]: a rotated key changes
/// only the contents of a Secret, and a Secret's contents are not part of a
/// Deployment's pod template, so without this nothing would restart and the
/// running daemon would keep the env value it booted with. The HASH, never the
/// key: an annotation is world-readable to anyone with `get deployment`.
pub const LLM_KEY_HASH_ANNOTATION: &str = "passband.email/llm-key-hash";

/// Annotation on the identity Secret recording when phase one ran, as Unix
/// seconds. The pending sweep needs an age, and this is the only place a
/// tenant's own record of one lives.
pub const CREATED_AT_ANNOTATION: &str = "passband.email/created-at";

/// Selects everything this warden created, for the pending sweep. Matches the
/// managed-by label [`labels`] puts on every tenant object.
pub const MANAGED_SELECTOR: &str = "app.kubernetes.io/managed-by=squelch-warden";

/// The only path prefixes an Ingress routes to a tenant daemon.
///
/// **This is how `/mcp` answers 404 at the ingress layer.** The daemon serves
/// four trees: `/client/*` (the human door's API), `/console/*` (the tenant's
/// own browser console), `/t/*` (the read-tracking pixel), and `/mcp` (the
/// agent door). Hosted publishes the human door only, so the Ingress declares
/// the human-door prefixes and nothing else. A request for `/mcp`, or for
/// anything else on a tenant vhost, matches no rule and the ingress controller
/// answers its own 404 (both Traefik and ingress-nginx do).
///
/// An allowlist, not a deny rule, and deliberately: a deny rule has to name
/// every spelling of the thing it is refusing, and it fails open when it misses
/// one. This fails closed. The cost is that a new unauthenticated route in
/// `squelch-api` needs a line here, which is a compile-time-visible list rather
/// than a controller annotation nobody reads. `/console` is that cost being
/// paid for the first time.
pub const HUMAN_DOOR_PREFIXES: &[&str] = &["/client", "/console", "/t"];

/// Namespace CoreDNS runs in on k3s. The one egress peer every tenant needs
/// before it can resolve anything.
pub const DNS_NAMESPACE: &str = "kube-system";

/// Namespace the metrics collectors run in, and the prometheus-agent pod's own
/// label. The single peer allowed to reach [`METRICS_PORT`].
///
/// Hardcoded rather than configured because the pod on the other side is
/// installed from this repo (`deploy/hosted/80-monitoring.yaml`): these two
/// values and that manifest's namespace and pod labels are one contract, and a
/// knob would only let them drift apart in an install nobody tests.
pub const MONITORING_NAMESPACE: &str = "monitoring";
/// See [`MONITORING_NAMESPACE`]. The label on the prometheus-agent pod template,
/// which is also what its own Deployment selects on.
pub const MONITORING_POD_LABEL: (&str, &str) = ("app.kubernetes.io/name", "prometheus-agent");

/// The auto-applied label Kubernetes puts on every namespace, which is how a
/// `namespaceSelector` names one without the warden needing to label it.
pub const NAMESPACE_NAME_LABEL: &str = "kubernetes.io/metadata.name";

/// Networks a tenant may NOT reach, subtracted from its internet egress.
///
/// This list is the "no tenant-to-tenant reachability" claim in one place. On
/// k3s the pod CIDR is 10.42.0.0/16 and the service CIDR is 10.43.0.0/16, so
/// excluding 10.0.0.0/8 removes every other tenant's pod, the warden's pod and
/// Service, and the in-cluster `kubernetes` API Service at 10.43.0.1 in one
/// stroke. The rest are the other RFC 1918 ranges, the CGNAT range, and
/// link-local, which is where every cloud's instance-metadata endpoint lives.
pub const BLOCKED_EGRESS_CIDRS: &[&str] = &[
    "10.0.0.0/8",
    "172.16.0.0/12",
    "192.168.0.0/16",
    "169.254.0.0/16",
    "100.64.0.0/10",
];

/// What the init container runs before the daemon starts.
///
/// A constant with no interpolation of any kind: no tenant label, no path from
/// config, nothing a caller can influence. It is a shell script only because
/// copying two files is not worth an image of its own.
///
/// It exists for one reason that is not optional: **the credentials file has to
/// be writable.** `FileCredentialStore` rewrites it on every token refresh, and
/// a Secret volume is read-only, so a daemon pointed straight at the Secret
/// mount would sync happily for an hour and then fail forever.
///
/// The copy is neither unconditional nor no-clobber, and both would be wrong:
///
/// - unconditional throws away the refreshed credential on every restart, and
///   Google does rotate refresh tokens;
/// - no-clobber makes seeding one-shot forever, so a re-consent stored by
///   `PUT /credentials` reaches the Secret and never reaches the daemon.
///
/// So it keeps a marker ([`CREDENTIAL_MARKER_PATH`]) holding the SHA-256 of the
/// blob it last installed, and copies only when the mounted Secret hashes to
/// something else. A daemon-rewritten file survives every restart; a genuinely
/// new ciphertext installs on the roll that
/// [`CREDENTIAL_HASH_ANNOTATION`] triggers.
///
/// The second half is the embedding weights, and it is a no-op unless the
/// operator configured a model PVC (see `deploy/hosted/SETUP.md`). Copied
/// rather than symlinked because the daemon's root filesystem is read-only and
/// fastembed expects to own its cache directory.
pub const SEED_SCRIPT: &str = "set -eu\n\
    want=$(sha256sum /etc/squelch/credential/credentials.json | cut -d ' ' -f 1)\n\
    have=''\n\
    if [ -f /data/.credentials.seed ]; then have=$(cat /data/.credentials.seed); fi\n\
    if [ \"$want\" != \"$have\" ]; then\n\
    \x20 cp /etc/squelch/credential/credentials.json /data/credentials.json\n\
    \x20 chmod 600 /data/credentials.json\n\
    \x20 printf '%s\\n' \"$want\" > /data/.credentials.seed\n\
    fi\n\
    if [ -d /models ] && [ ! -d /data/.local/share/squelch/models ]; then\n\
    \x20 mkdir -p /data/.local/share/squelch\n\
    \x20 cp -r /models /data/.local/share/squelch/models\n\
    fi\n";

/// The init container's bounds. Constants rather than config: it copies at most
/// two files and exits, and nothing about a tenant changes what that costs. They
/// exist so the pod has no unbounded container, not because anyone will ever
/// want to tune them. The ceiling is generous enough for a 130 MB `cp -r` of
/// the weights and small enough that a stuck one cannot take the node.
pub fn seed_resources() -> Resources {
    Resources {
        cpu_request: "10m".to_string(),
        cpu_limit: "500m".to_string(),
        memory_request: "16Mi".to_string(),
        memory_limit: "64Mi".to_string(),
        // Both volumes it touches are volumes; this covers its own logs.
        ephemeral_request: "16Mi".to_string(),
        ephemeral_limit: "64Mi".to_string(),
    }
}

/// A container's `resources`, as Kubernetes wants them.
fn requirements(resources: &Resources) -> ResourceRequirements {
    ResourceRequirements {
        requests: Some(BTreeMap::from([
            ("cpu".to_string(), Quantity(resources.cpu_request.clone())),
            (
                "memory".to_string(),
                Quantity(resources.memory_request.clone()),
            ),
            (
                "ephemeral-storage".to_string(),
                Quantity(resources.ephemeral_request.clone()),
            ),
        ])),
        limits: Some(BTreeMap::from([
            ("cpu".to_string(), Quantity(resources.cpu_limit.clone())),
            (
                "memory".to_string(),
                Quantity(resources.memory_limit.clone()),
            ),
            (
                "ephemeral-storage".to_string(),
                Quantity(resources.ephemeral_limit.clone()),
            ),
        ])),
        ..Default::default()
    }
}

/// The SHA-256 of a credential ciphertext, hex, for
/// [`CREDENTIAL_HASH_ANNOTATION`].
///
/// A hash of the ARMORED text exactly as the control plane sent it, which is
/// also exactly what lands in the Secret and what the init container hashes on
/// the other side. Nothing here parses or normalises it: two blobs are the same
/// blob only if they are byte-identical.
pub fn credential_hash(ciphertext: &str) -> String {
    let digest = Sha256::digest(ciphertext.as_bytes());
    digest.iter().map(|b| format!("{b:02x}")).collect()
}

/// The pod-roll hash for the LLM Secret, over BOTH of the keys it can carry.
///
/// [`LLM_KEY_HASH_ANNOTATION`] must move when EITHER credential in
/// `<label>-llm` changes, or rotating one would leave the pod running on the
/// stale pair. A hash of hashes joined by a newline rather than a hash of
/// concatenated keys, so no choice of key bytes can make two different pairs
/// read as one (the inner digests are fixed-width hex, which no boundary
/// shift survives). An absent slot — either slot: a half-failed mint can
/// leave the Secret holding only one of the two — contributes the empty
/// string, distinct from every minted key's digest, and unambiguous because
/// [`crate::validate::validate_llm_api_key`] refuses an empty key, so
/// "absent" and "empty" are the same state by construction.
///
/// Both computations of this hash — `set_llm_key` with the keys in hand, and
/// `set_credentials` reading them back out of the stored Secret — go through
/// this one function so they cannot diverge and spuriously roll (or fail to
/// roll) a pod.
pub fn llm_keys_hash(api_key: Option<&str>, assistant_api_key: Option<&str>) -> String {
    let triage = api_key.map(credential_hash).unwrap_or_default();
    let assistant = assistant_api_key.map(credential_hash).unwrap_or_default();
    credential_hash(&format!("{triage}\n{assistant}"))
}

/// Labels every object of a tenant carries.
pub fn labels(name: &TenantName) -> BTreeMap<String, String> {
    let mut labels = selector_labels(name);
    labels.insert(
        "app.kubernetes.io/managed-by".to_string(),
        "squelch-warden".to_string(),
    );
    labels
}

/// The subset that selects a tenant's pods. Kept separate because a
/// Deployment's selector is immutable after creation: adding a label to
/// [`labels`] must never change what an existing Deployment selects.
pub fn selector_labels(name: &TenantName) -> BTreeMap<String, String> {
    BTreeMap::from([
        ("app.kubernetes.io/name".to_string(), "squelchd".to_string()),
        (
            "app.kubernetes.io/instance".to_string(),
            name.as_str().to_string(),
        ),
    ])
}

/// The label selector, as the API wants it on a list request.
pub fn pod_selector(name: &TenantName) -> String {
    format!(
        "app.kubernetes.io/name=squelchd,app.kubernetes.io/instance={}",
        name.as_str()
    )
}

fn meta(config: &Config, name: &TenantName, object_name: String) -> ObjectMeta {
    ObjectMeta {
        name: Some(object_name),
        namespace: Some(config.namespace.clone()),
        labels: Some(labels(name)),
        ..Default::default()
    }
}

/// The tenant's identity Secret: its age key pair and its mailbox address.
///
/// Created in phase one, before the recipient is handed back, so the control
/// plane can never hold a recipient whose identity failed to persist. The only
/// Secret this service ever deletes, and only through the pending sweep
/// ([`crate::provision::Warden::sweep_pending`]): losing it once a tenant is
/// running means every credential sealed to that tenant is unrecoverable.
///
/// `created_at` is Unix seconds, and it is what the sweep ages a pending tenant
/// by; see [`CREATED_AT_ANNOTATION`].
pub fn identity_secret(
    config: &Config,
    name: &TenantName,
    identity: &TenantIdentity,
    account_email: &str,
    created_at: u64,
) -> Secret {
    let mut metadata = meta(config, name, name.identity_secret());
    metadata.annotations = Some(BTreeMap::from([(
        CREATED_AT_ANNOTATION.to_string(),
        created_at.to_string(),
    )]));

    Secret {
        metadata,
        type_: Some("Opaque".to_string()),
        // `string_data` rather than `data`: the API server does the base64, so
        // no code here handles a key as bytes it might log by accident.
        string_data: Some(BTreeMap::from([
            (IDENTITY_KEY.to_string(), identity.expose().to_string()),
            (RECIPIENT_KEY.to_string(), identity.recipient().to_string()),
            (ACCOUNT_EMAIL_KEY.to_string(), account_email.to_string()),
        ])),
        ..Default::default()
    }
}

/// The tenant's credential Secret: the age-armored blob the control plane
/// sealed to this tenant's recipient, verbatim.
///
/// The warden cannot read it. It kept no identity after
/// [`identity_secret`] was applied, and it never had this tenant's plaintext.
pub fn credential_secret(config: &Config, name: &TenantName, ciphertext: &str) -> Secret {
    Secret {
        metadata: meta(config, name, name.credential_secret()),
        type_: Some("Opaque".to_string()),
        string_data: Some(BTreeMap::from([(
            CREDENTIAL_KEY.to_string(),
            ciphertext.to_string(),
        )])),
        ..Default::default()
    }
}

/// The tenant's LLM Secret: the gateway virtual keys the control plane minted
/// for this tenant, verbatim — whichever of the triage and assistant relay
/// slots exist, and at least one (see [`ASSISTANT_API_KEY_KEY`]).
///
/// Unlike the credential blob, these are plaintext to the warden: they are
/// embedded in this Secret object and read back to be hashed for the roll
/// annotation ([`LLM_KEY_HASH_ANNOTATION`]). What holds instead: neither key
/// is ever logged or returned on the wire, and only the combined hash lands on
/// the world-readable pod spec — the daemon gets the keys themselves as
/// `secretKeyRef`s (see [`daemon_env`]).
pub fn llm_secret(
    config: &Config,
    name: &TenantName,
    api_key: Option<&str>,
    assistant_api_key: Option<&str>,
) -> Secret {
    // Only the slots that exist: the apply force-owns this Secret, so a data
    // key rendered here is the whole set that survives. The caller
    // ([`crate::provision::Warden::set_llm_key`]) always passes the union of
    // the request and what was stored, and at least one slot.
    let mut string_data = BTreeMap::new();
    if let Some(api_key) = api_key {
        string_data.insert(LLM_API_KEY_KEY.to_string(), api_key.to_string());
    }
    if let Some(assistant) = assistant_api_key {
        string_data.insert(ASSISTANT_API_KEY_KEY.to_string(), assistant.to_string());
    }
    Secret {
        metadata: meta(config, name, name.llm_secret()),
        type_: Some("Opaque".to_string()),
        string_data: Some(string_data),
        ..Default::default()
    }
}

/// The tenant's data volume. `ReadWriteOnce` on k3s local-path: one node, one
/// writer, and the Deployment's `Recreate` strategy is what keeps it that way
/// across an image change.
pub fn data_pvc(config: &Config, name: &TenantName) -> PersistentVolumeClaim {
    PersistentVolumeClaim {
        metadata: meta(config, name, name.data_pvc()),
        spec: Some(PersistentVolumeClaimSpec {
            access_modes: Some(vec!["ReadWriteOnce".to_string()]),
            storage_class_name: Some(config.storage_class.clone()),
            resources: Some(VolumeResourceRequirements {
                requests: Some(BTreeMap::from([(
                    "storage".to_string(),
                    Quantity(config.storage_size.clone()),
                )])),
                ..Default::default()
            }),
            ..Default::default()
        }),
        ..Default::default()
    }
}

/// The tenant's Service. ClusterIP, one port, reachable only by whatever the
/// NetworkPolicy allows.
pub fn service(config: &Config, name: &TenantName) -> Service {
    Service {
        metadata: meta(config, name, name.as_str().to_string()),
        spec: Some(ServiceSpec {
            type_: Some("ClusterIP".to_string()),
            selector: Some(selector_labels(name)),
            ports: Some(vec![ServicePort {
                name: Some("http".to_string()),
                port: DAEMON_PORT,
                target_port: Some(IntOrString::String("http".to_string())),
                protocol: Some("TCP".to_string()),
                ..Default::default()
            }]),
            ..Default::default()
        }),
        ..Default::default()
    }
}

/// The tenant's Ingress: `<label>.<base domain>` over the wildcard certificate,
/// routing the human door and nothing else.
///
/// See [`HUMAN_DOOR_PREFIXES`] for why the agent door is absent rather than
/// denied.
pub fn ingress(config: &Config, name: &TenantName) -> Ingress {
    let backend = IngressBackend {
        service: Some(IngressServiceBackend {
            name: name.as_str().to_string(),
            port: Some(ServiceBackendPort {
                name: Some("http".to_string()),
                ..Default::default()
            }),
        }),
        ..Default::default()
    };
    let host = config.hostname(name.as_str());

    Ingress {
        metadata: meta(config, name, name.as_str().to_string()),
        spec: Some(IngressSpec {
            ingress_class_name: Some(config.ingress_class.clone()),
            tls: Some(vec![IngressTLS {
                hosts: Some(vec![host.clone()]),
                secret_name: Some(config.tls_secret.clone()),
            }]),
            rules: Some(vec![IngressRule {
                host: Some(host),
                http: Some(HTTPIngressRuleValue {
                    paths: HUMAN_DOOR_PREFIXES
                        .iter()
                        .map(|prefix| HTTPIngressPath {
                            path: Some((*prefix).to_string()),
                            path_type: "Prefix".to_string(),
                            backend: backend.clone(),
                        })
                        // The bare root, EXACT on purpose: it serves one
                        // redirect to /console and nothing else. A Prefix "/"
                        // would match EVERY path — including /mcp — and turn
                        // this allowlist into decoration.
                        .chain(std::iter::once(HTTPIngressPath {
                            path: Some("/".to_string()),
                            path_type: "Exact".to_string(),
                            backend: backend.clone(),
                        }))
                        .collect(),
                }),
            }]),
            ..Default::default()
        }),
        ..Default::default()
    }
}

/// The tenant's NetworkPolicy: the isolation claim, written down.
///
/// Ingress: nothing may open a connection to a tenant pod except the ingress
/// controller, on the daemon port, and the monitoring namespace's
/// prometheus-agent, on the metrics port. Not other tenants, not the warden,
/// not anything else in the cluster.
///
/// Egress: DNS, and TCP 443 to the public internet minus
/// [`BLOCKED_EGRESS_CIDRS`]. That is Gmail and Anthropic and the model
/// download, and it is not the API server, not the warden, and not another
/// tenant's pod. Note what selecting both `policyTypes` does: an empty rule
/// list would be deny-all, and these two lists are the entire allowance.
///
/// The second ingress rule is the metrics allowance, and it is worth stating
/// precisely because this is the isolation claim: one pod
/// ([`MONITORING_POD_LABEL`]) in one namespace ([`MONITORING_NAMESPACE`]) may
/// open a connection to [`METRICS_PORT`] and to nothing else. It cannot reach
/// 8848, which stays sealed to the ingress controller, so the agent can read a
/// tenant's counters and cannot touch a tenant's mail; and no other tenant gains
/// anything, because a peer is a namespace AND a pod selector, not either. The
/// metrics port is absent from the Service and the Ingress, so this rule is the
/// only path to it that exists.
///
/// One optional third ingress rule: the NODE, when
/// [`crate::config::Config::node_cidr`] is set. A kubelet readiness probe is a
/// connection from the node's own address, which matches no pod and no
/// namespace, so a CNI that does not exempt host-originated traffic drops every
/// probe and every provision times out on a pod that is perfectly healthy. The
/// hole is one CIDR to one port, and it lets in node-originated traffic only:
/// another tenant's pod still arrives from a pod IP and is still refused.
pub fn network_policy(config: &Config, name: &TenantName) -> NetworkPolicy {
    let (ingress_label_key, ingress_label_value) = config.ingress_pod_label.clone();

    let daemon_port = vec![NetworkPolicyPort {
        port: Some(IntOrString::Int(DAEMON_PORT)),
        protocol: Some("TCP".to_string()),
        ..Default::default()
    }];
    let mut ingress = vec![NetworkPolicyIngressRule {
        from: Some(vec![NetworkPolicyPeer {
            namespace_selector: Some(LabelSelector {
                match_labels: Some(BTreeMap::from([(
                    NAMESPACE_NAME_LABEL.to_string(),
                    config.ingress_namespace.clone(),
                )])),
                ..Default::default()
            }),
            pod_selector: Some(LabelSelector {
                match_labels: Some(BTreeMap::from([(ingress_label_key, ingress_label_value)])),
                ..Default::default()
            }),
            ..Default::default()
        }]),
        ports: Some(daemon_port.clone()),
    }];
    ingress.push(NetworkPolicyIngressRule {
        from: Some(vec![NetworkPolicyPeer {
            namespace_selector: Some(LabelSelector {
                match_labels: Some(BTreeMap::from([(
                    NAMESPACE_NAME_LABEL.to_string(),
                    MONITORING_NAMESPACE.to_string(),
                )])),
                ..Default::default()
            }),
            pod_selector: Some(LabelSelector {
                match_labels: Some(BTreeMap::from([(
                    MONITORING_POD_LABEL.0.to_string(),
                    MONITORING_POD_LABEL.1.to_string(),
                )])),
                ..Default::default()
            }),
            ..Default::default()
        }]),
        ports: Some(vec![NetworkPolicyPort {
            port: Some(IntOrString::Int(METRICS_PORT)),
            protocol: Some("TCP".to_string()),
            ..Default::default()
        }]),
    });
    if let Some(node_cidr) = &config.node_cidr {
        ingress.push(NetworkPolicyIngressRule {
            from: Some(vec![NetworkPolicyPeer {
                ip_block: Some(IPBlock {
                    cidr: node_cidr.clone(),
                    except: None,
                }),
                ..Default::default()
            }]),
            ports: Some(daemon_port),
        });
    }

    NetworkPolicy {
        metadata: meta(config, name, name.as_str().to_string()),
        spec: Some(NetworkPolicySpec {
            pod_selector: Some(LabelSelector {
                match_labels: Some(selector_labels(name)),
                ..Default::default()
            }),
            policy_types: Some(vec!["Ingress".to_string(), "Egress".to_string()]),
            ingress: Some(ingress),
            egress: Some(vec![
                // CoreDNS, and only CoreDNS. A tenant that could reach any
                // resolver could exfiltrate over DNS to one it chose.
                NetworkPolicyEgressRule {
                    to: Some(vec![NetworkPolicyPeer {
                        namespace_selector: Some(LabelSelector {
                            match_labels: Some(BTreeMap::from([(
                                NAMESPACE_NAME_LABEL.to_string(),
                                DNS_NAMESPACE.to_string(),
                            )])),
                            ..Default::default()
                        }),
                        pod_selector: Some(LabelSelector {
                            match_labels: Some(BTreeMap::from([(
                                "k8s-app".to_string(),
                                "kube-dns".to_string(),
                            )])),
                            ..Default::default()
                        }),
                        ..Default::default()
                    }]),
                    ports: Some(vec![
                        NetworkPolicyPort {
                            port: Some(IntOrString::Int(53)),
                            protocol: Some("UDP".to_string()),
                            ..Default::default()
                        },
                        NetworkPolicyPort {
                            port: Some(IntOrString::Int(53)),
                            protocol: Some("TCP".to_string()),
                            ..Default::default()
                        },
                    ]),
                },
                // Gmail, Anthropic, the relay, the one-time weights download.
                NetworkPolicyEgressRule {
                    to: Some(vec![NetworkPolicyPeer {
                        ip_block: Some(IPBlock {
                            cidr: "0.0.0.0/0".to_string(),
                            except: Some(
                                BLOCKED_EGRESS_CIDRS
                                    .iter()
                                    .map(|c| (*c).to_string())
                                    .collect(),
                            ),
                        }),
                        ..Default::default()
                    }]),
                    ports: Some(vec![NetworkPolicyPort {
                        port: Some(IntOrString::Int(443)),
                        protocol: Some("TCP".to_string()),
                        ..Default::default()
                    }]),
                },
            ]),
        }),
    }
}

/// The tenant's Deployment.
///
/// One replica, `Recreate`, and a pod that gives up everything it can:
/// non-root at a fixed high uid, no privilege escalation, every capability
/// dropped, the runtime's default seccomp profile, a read-only root filesystem
/// with writable mounts for the data volume and `/tmp` and nothing else, no
/// ServiceAccount token, no service-link environment, and (unless the operator
/// turned it off) its own user namespace.
///
/// The image's entrypoint is deliberately bypassed: `docker-entrypoint.sh`
/// starts as root to chown the volume and then drops with `setpriv`, which
/// cannot work under `runAsNonRoot`. `fsGroup` does that chown instead, at the
/// kubelet, before the container starts.
///
/// `credential_hash` is [`credential_hash`] of the ciphertext this tenant is
/// being provisioned with. It rides in an annotation on the pod template so a
/// re-consent actually reaches the daemon; see [`CREDENTIAL_HASH_ANNOTATION`].
///
/// `llm_key_hash` is the same mechanism for the LLM virtual key, when this
/// tenant has one: absent for a tenant never keyed, present so a rotated key
/// is a changed pod spec and a roll. See [`LLM_KEY_HASH_ANNOTATION`].
pub fn deployment(
    config: &Config,
    name: &TenantName,
    credential_hash: &str,
    llm_key_hash: Option<&str>,
) -> Deployment {
    let pod_security = PodSecurityContext {
        run_as_non_root: Some(true),
        run_as_user: Some(config.run_as),
        run_as_group: Some(config.run_as),
        // What makes the PersistentVolume writable by an unprivileged uid
        // without anything in the pod ever being root.
        fs_group: Some(config.run_as),
        // `Always` (the default) walks and chowns the whole volume on every
        // start. A mailbox index plus an embedding cache is a lot of inodes to
        // touch before a restart can begin serving, and the ownership only ever
        // changes if `run_as` does. `OnRootMismatch` checks the top directory
        // and skips the walk when it already matches.
        fs_group_change_policy: Some("OnRootMismatch".to_string()),
        seccomp_profile: Some(SeccompProfile {
            type_: "RuntimeDefault".to_string(),
            ..Default::default()
        }),
        ..Default::default()
    };
    let container_security = SecurityContext {
        allow_privilege_escalation: Some(false),
        read_only_root_filesystem: Some(true),
        run_as_non_root: Some(true),
        capabilities: Some(Capabilities {
            drop: Some(vec!["ALL".to_string()]),
            ..Default::default()
        }),
        ..Default::default()
    };

    let data_mount = VolumeMount {
        name: "data".to_string(),
        mount_path: DATA_DIR.to_string(),
        ..Default::default()
    };
    let tmp_mount = VolumeMount {
        name: "tmp".to_string(),
        mount_path: "/tmp".to_string(),
        ..Default::default()
    };

    let mut volumes = vec![
        Volume {
            name: "data".to_string(),
            persistent_volume_claim: Some(PersistentVolumeClaimVolumeSource {
                claim_name: name.data_pvc(),
                ..Default::default()
            }),
            ..Default::default()
        },
        Volume {
            name: "identity".to_string(),
            secret: Some(SecretVolumeSource {
                secret_name: Some(name.identity_secret()),
                // Projected key by key: the Secret also holds the recipient and
                // the mailbox address, and the daemon has no business seeing a
                // file it did not ask for.
                items: Some(vec![KeyToPath {
                    key: IDENTITY_KEY.to_string(),
                    path: IDENTITY_KEY.to_string(),
                    ..Default::default()
                }]),
                default_mode: Some(0o400),
                ..Default::default()
            }),
            ..Default::default()
        },
        Volume {
            name: "credential".to_string(),
            secret: Some(SecretVolumeSource {
                secret_name: Some(name.credential_secret()),
                items: Some(vec![KeyToPath {
                    key: CREDENTIAL_KEY.to_string(),
                    path: CREDENTIAL_KEY.to_string(),
                    ..Default::default()
                }]),
                default_mode: Some(0o400),
                ..Default::default()
            }),
            ..Default::default()
        },
        Volume {
            name: "tmp".to_string(),
            empty_dir: Some(EmptyDirVolumeSource {
                // An emptyDir with no sizeLimit is a hole with the node's free
                // space at the bottom of it, shared with every other tenant.
                size_limit: Some(Quantity(config.tmp_size.clone())),
                ..Default::default()
            }),
            ..Default::default()
        },
    ];

    let mut seed_mounts = vec![
        data_mount.clone(),
        VolumeMount {
            name: "credential".to_string(),
            mount_path: CREDENTIAL_SEED_DIR.to_string(),
            read_only: Some(true),
            ..Default::default()
        },
    ];

    if let Some(model_pvc) = &config.model_pvc {
        volumes.push(Volume {
            name: "models".to_string(),
            persistent_volume_claim: Some(PersistentVolumeClaimVolumeSource {
                claim_name: model_pvc.clone(),
                read_only: Some(true),
            }),
            ..Default::default()
        });
        seed_mounts.push(VolumeMount {
            name: "models".to_string(),
            mount_path: MODELS_DIR.to_string(),
            read_only: Some(true),
            ..Default::default()
        });
    }

    let seed = Container {
        name: "seed".to_string(),
        image: Some(config.image.clone()),
        command: Some(vec!["/bin/sh".to_string(), "-c".to_string()]),
        args: Some(vec![SEED_SCRIPT.to_string()]),
        volume_mounts: Some(seed_mounts),
        security_context: Some(container_security.clone()),
        resources: Some(requirements(&seed_resources())),
        ..Default::default()
    };

    let daemon = Container {
        name: "squelchd".to_string(),
        image: Some(config.image.clone()),
        // The image's ENTRYPOINT is root-then-setpriv; see the doc comment.
        command: Some(vec!["/usr/local/bin/squelchd".to_string()]),
        args: Some(vec!["serve".to_string()]),
        ports: Some(vec![
            ContainerPort {
                name: Some("http".to_string()),
                container_port: DAEMON_PORT,
                protocol: Some("TCP".to_string()),
                ..Default::default()
            },
            // Declared so prometheus-agent's pod discovery can keep on a port
            // NAME rather than a number. Naming a port here publishes nothing:
            // the Service does not carry it, so it is reachable only through
            // the NetworkPolicy rule. See [`METRICS_PORT`].
            ContainerPort {
                name: Some("metrics".to_string()),
                container_port: METRICS_PORT,
                protocol: Some("TCP".to_string()),
                ..Default::default()
            },
        ]),
        env: Some(daemon_env(config, name)),
        volume_mounts: Some(vec![
            data_mount,
            VolumeMount {
                name: "identity".to_string(),
                mount_path: IDENTITY_DIR.to_string(),
                read_only: Some(true),
                ..Default::default()
            },
            tmp_mount,
        ]),
        // A TCP accept, not an HTTP GET: the daemon serves no unauthenticated
        // health route, and a probe that has to hold a bearer token is a
        // credential in a pod spec.
        readiness_probe: Some(Probe {
            tcp_socket: Some(TCPSocketAction {
                port: IntOrString::Int(DAEMON_PORT),
                ..Default::default()
            }),
            initial_delay_seconds: Some(2),
            period_seconds: Some(3),
            ..Default::default()
        }),
        security_context: Some(container_security),
        resources: Some(requirements(&config.daemon_resources)),
        ..Default::default()
    };

    Deployment {
        metadata: meta(config, name, name.as_str().to_string()),
        spec: Some(DeploymentSpec {
            replicas: Some(1),
            // Never two daemons on one SQLite file: the volume is
            // ReadWriteOnce, and a rolling update would try to start the new
            // pod before the old one released it.
            strategy: Some(DeploymentStrategy {
                type_: Some("Recreate".to_string()),
                ..Default::default()
            }),
            selector: LabelSelector {
                match_labels: Some(selector_labels(name)),
                ..Default::default()
            },
            template: PodTemplateSpec {
                metadata: Some(ObjectMeta {
                    labels: Some(labels(name)),
                    // What makes a re-consent (and a key rotation) land: see
                    // [`CREDENTIAL_HASH_ANNOTATION`] and
                    // [`LLM_KEY_HASH_ANNOTATION`].
                    annotations: Some({
                        let mut annotations = BTreeMap::from([(
                            CREDENTIAL_HASH_ANNOTATION.to_string(),
                            credential_hash.to_string(),
                        )]);
                        if let Some(hash) = llm_key_hash {
                            annotations
                                .insert(LLM_KEY_HASH_ANNOTATION.to_string(), hash.to_string());
                        }
                        annotations
                    }),
                    ..Default::default()
                }),
                spec: Some(PodSpec {
                    // Per-tenant user namespace: root inside the pod is an
                    // unprivileged id on the node. `None` when the operator
                    // turned it off, because sending `true` would ask for the
                    // host's user namespace explicitly, which is the opposite
                    // of what "unsupported cluster" means.
                    host_users: config.user_namespaces.then_some(false),
                    security_context: Some(pod_security),
                    // A tenant pod has no business talking to the API server,
                    // and no reason to learn the names of other Services.
                    automount_service_account_token: Some(false),
                    enable_service_links: Some(false),
                    init_containers: Some(vec![seed]),
                    containers: vec![daemon],
                    volumes: Some(volumes),
                    image_pull_secrets: config
                        .pull_secret
                        .as_ref()
                        .map(|name| vec![LocalObjectReference { name: name.clone() }]),
                    ..Default::default()
                }),
            },
            ..Default::default()
        }),
        ..Default::default()
    }
}

/// The daemon's environment. These are the REAL variable names `squelch-core`
/// reads, checked against `squelch-core/src/config.rs`; v1 shipped two that the
/// daemon never looked at.
fn daemon_env(config: &Config, name: &TenantName) -> Vec<EnvVar> {
    let plain = |key: &str, value: &str| EnvVar {
        name: key.to_string(),
        value: Some(value.to_string()),
        ..Default::default()
    };
    let from_secret = |key: &str, secret: String, data_key: &str| EnvVar {
        name: key.to_string(),
        value_from: Some(EnvVarSource {
            secret_key_ref: Some(SecretKeySelector {
                name: secret,
                key: data_key.to_string(),
                ..Default::default()
            }),
            ..Default::default()
        }),
        ..Default::default()
    };
    // The same, but `optional: true`: the kubelet leaves the variable unset when
    // the Secret or the key is missing instead of holding the pod in
    // `CreateContainerConfigError` forever.
    let from_optional_secret = |key: &str, secret: String, data_key: &str| EnvVar {
        name: key.to_string(),
        value_from: Some(EnvVarSource {
            secret_key_ref: Some(SecretKeySelector {
                name: secret,
                key: data_key.to_string(),
                optional: Some(true),
            }),
            ..Default::default()
        }),
        ..Default::default()
    };
    let mut env = vec![
        // Inside the pod's own network namespace. Not reachable from anywhere
        // the NetworkPolicy does not allow.
        plain("SQUELCH_BIND", "0.0.0.0:8848"),
        // Turns on the metrics listener, which is off unless this is set. A
        // second port, unauthenticated, and reachable only from
        // prometheus-agent: see [`METRICS_PORT`] and [`network_policy`].
        plain("SQUELCH_METRICS_BIND", "0.0.0.0:9464"),
        plain("SQUELCH_DB_PATH", "/data/squelch.db"),
        plain("SQUELCH_CRED_BACKEND", "file"),
        plain("SQUELCH_CREDENTIALS_PATH", CREDENTIAL_PATH),
        plain("SQUELCH_CRED_AGE_IDENTITY", IDENTITY_FILE),
        // Where the embedding-weights cache resolves to
        // (`$HOME/.local/share/squelch/models`), and where config.toml
        // discovery starts. On the tenant's own volume, so neither escapes it.
        plain("HOME", DATA_DIR),
        // From the Secret, never from the pod spec: see [`ACCOUNT_EMAIL_KEY`].
        from_secret(
            "SQUELCH_ACCOUNT_EMAIL",
            name.identity_secret(),
            ACCOUNT_EMAIL_KEY,
        ),
        // The OAuth client the tenant's refresh token was minted by, without
        // which the daemon refuses to start (`Config::oauth_client`) and could
        // not refresh an access token if it did.
        //
        // This is the control plane's CONFIDENTIAL WEB client, and its secret
        // being inside a tenant pod is deliberate: a refresh token only works
        // for the client that minted it, hosted daemons are our own
        // infrastructure, and this is the custody hosted admits to. Shared by
        // every tenant, because it is one Google project's one web client. See
        // `deploy/hosted/SETUP.md`, "The Google OAuth client".
        from_secret(
            "SQUELCH_CLIENT_ID",
            config.oauth_secret_name.clone(),
            OAUTH_CLIENT_ID_KEY,
        ),
        from_secret(
            "SQUELCH_CLIENT_SECRET",
            config.oauth_secret_name.clone(),
            OAUTH_CLIENT_SECRET_KEY,
        ),
    ];

    // The control plane's origin, behind the console's "Continue with Google"
    // link. ABSENT when the operator did not configure one, rather than empty:
    // the daemon treats a blank value as unset anyway, but an empty env var in a
    // pod spec reads like a setting somebody meant to fill in.
    if let Some(url) = &config.console_sso_url {
        env.push(plain("SQUELCH_CONSOLE_SSO_URL", url));
    }

    // The LLM gateway block, present only when the operator configured one.
    // With it absent the env above is byte-identical to what a warden without
    // the feature built, which is the env-contract tests' whole claim.
    if let Some(base_url) = &config.llm_base_url {
        env.push(plain("SQUELCH_ANTHROPIC_BASE_URL", base_url));
        // Belt and braces: the base URL alone would already select Anthropic's
        // wire shape, but the daemon also sniffs the provider from the model
        // name, and a gateway-side model alias must not flip a tenant onto a
        // provider path the gateway does not speak.
        env.push(plain("SQUELCH_STAGE2_PROVIDER", "anthropic"));
        // The virtual key, from the Secret, never inline — and OPTIONAL: a
        // tenant whose key was never minted must still boot. An unminted
        // tenant resolves no key at all, and the daemon fail-softs to
        // heuristic-only triage until `squelch-control llm mint` runs.
        env.push(from_optional_secret(
            "SQUELCH_STAGE2_API_KEY",
            name.llm_secret(),
            LLM_API_KEY_KEY,
        ));
        // The assistant relay credential, and OPTIONAL for a second reason on
        // top of the triage key's: a tenant minted before the assistant era
        // has an LLM Secret with no such data key at all, and it must still
        // boot — the daemon's assistant route answers unavailable until the
        // control plane mints the key.
        env.push(from_optional_secret(
            "SQUELCH_ASSISTANT_API_KEY",
            name.llm_secret(),
            ASSISTANT_API_KEY_KEY,
        ));
        if let Some(model) = &config.llm_stage1_model {
            env.push(plain("SQUELCH_STAGE1_MODEL", model));
        }
        if let Some(model) = &config.llm_stage2_model {
            // Stage 2's model variable really is `SQUELCH_MODEL`: it predates
            // the stage split and the daemon never grew a prefixed alias.
            env.push(plain("SQUELCH_MODEL", model));
        }
        if let Some(cap) = config.llm_stage1_daily_cap {
            env.push(plain("SQUELCH_STAGE1_GLOBAL_DAILY_CAP", &cap.to_string()));
        }
        if let Some(cap) = config.llm_stage2_daily_cap {
            env.push(plain("SQUELCH_STAGE2_GLOBAL_DAILY_CAP", &cap.to_string()));
        }
    }
    env
}

/// The argv `squelchd pair` runs as inside a tenant pod.
///
/// Everything the command needs beyond this is already in the container's
/// environment, which an exec session inherits: the store path, the account,
/// and `HOME`. The URL is passed explicitly because the daemon cannot know its
/// own public hostname.
pub fn pair_argv(config: &Config, name: &TenantName) -> Vec<String> {
    vec![
        "/usr/local/bin/squelchd".to_string(),
        "pair".to_string(),
        "--url".to_string(),
        config.tenant_url(name.as_str()),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testing::test_config;

    fn name() -> TenantName {
        TenantName::parse("alice").unwrap()
    }

    /// A tenant's Deployment as provisioning builds it, for the tests that do
    /// not care which ciphertext it was rolled for. No LLM key, which is also
    /// the shape every unkeyed tenant gets.
    fn tenant_deployment(config: &Config) -> Deployment {
        deployment(config, &name(), &credential_hash("ct"), None)
    }

    /// A config with the LLM gateway turned on and nothing else changed.
    fn llm_config() -> Config {
        let mut c = test_config();
        c.llm_base_url = Some("https://llm.passband.internal".to_string());
        c
    }

    #[test]
    fn every_object_lands_in_the_tenant_namespace_under_the_tenants_name() {
        let c = test_config();
        let n = name();
        let id = TenantIdentity::mint();

        let expect = |meta: &ObjectMeta, object_name: &str| {
            assert_eq!(meta.name.as_deref(), Some(object_name));
            assert_eq!(meta.namespace.as_deref(), Some("tenants"));
            assert_eq!(
                meta.labels.as_ref().unwrap()["app.kubernetes.io/instance"],
                "alice"
            );
            assert_eq!(
                meta.labels.as_ref().unwrap()["app.kubernetes.io/managed-by"],
                "squelch-warden"
            );
        };

        expect(
            &identity_secret(&c, &n, &id, "alice@example.com", 1_700_000_000).metadata,
            "alice-identity",
        );
        expect(
            &credential_secret(&c, &n, "ct").metadata,
            "alice-credential",
        );
        expect(&data_pvc(&c, &n).metadata, "alice-data");
        expect(&service(&c, &n).metadata, "alice");
        expect(&ingress(&c, &n).metadata, "alice");
        expect(&network_policy(&c, &n).metadata, "alice");
        expect(&tenant_deployment(&c).metadata, "alice");
    }

    #[test]
    fn the_identity_secret_carries_the_pair_the_address_and_its_age() {
        let c = test_config();
        let id = TenantIdentity::mint();
        let secret = identity_secret(&c, &name(), &id, "alice@example.com", 1_700_000_000);
        let data = secret.string_data.unwrap();
        assert_eq!(data[IDENTITY_KEY], id.expose());
        assert_eq!(data[RECIPIENT_KEY], id.recipient());
        assert_eq!(data[ACCOUNT_EMAIL_KEY], "alice@example.com");
        assert_eq!(secret.type_.as_deref(), Some("Opaque"));
        assert_eq!(data.len(), 3);
        // What the pending sweep ages a half-finished signup by.
        assert_eq!(
            secret.metadata.annotations.unwrap()[CREATED_AT_ANNOTATION],
            "1700000000"
        );
    }

    #[test]
    fn the_credential_secret_is_the_ciphertext_verbatim() {
        let c = test_config();
        let ct = crate::testing::armored("alice");
        let secret = credential_secret(&c, &name(), &ct);
        assert_eq!(secret.string_data.unwrap()[CREDENTIAL_KEY], ct);
    }

    /// The isolation claim, asserted field by field. Every line here is
    /// something a reviewer of the hosted pitch is entitled to check.
    #[test]
    fn the_pod_gives_up_everything_it_can() {
        let c = test_config();
        let d = tenant_deployment(&c);
        let pod = d.spec.unwrap().template.spec.unwrap();

        // User namespaces on by default.
        assert_eq!(pod.host_users, Some(false));
        assert_eq!(pod.automount_service_account_token, Some(false));
        assert_eq!(pod.enable_service_links, Some(false));

        let ctx = pod.security_context.unwrap();
        assert_eq!(ctx.run_as_non_root, Some(true));
        assert_eq!(ctx.run_as_user, Some(10001));
        assert_eq!(ctx.run_as_group, Some(10001));
        assert_eq!(ctx.fs_group, Some(10001));
        assert_eq!(
            ctx.fs_group_change_policy.as_deref(),
            Some("OnRootMismatch")
        );
        assert_eq!(ctx.seccomp_profile.unwrap().type_, "RuntimeDefault");

        // Both containers, not just the daemon: an init container with
        // capabilities is an init container that can be abused.
        let mut containers = pod.init_containers.clone().unwrap();
        containers.extend(pod.containers.clone());
        assert_eq!(containers.len(), 2);
        for container in &containers {
            let sec = container.security_context.clone().unwrap();
            assert_eq!(sec.allow_privilege_escalation, Some(false));
            assert_eq!(sec.read_only_root_filesystem, Some(true));
            assert_eq!(sec.run_as_non_root, Some(true));
            assert_eq!(sec.capabilities.unwrap().drop.unwrap(), vec!["ALL"]);
        }

        // Read-only root means the writable mounts are the whole list.
        let daemon = &pod.containers[0];
        let mut writable: Vec<&str> = daemon
            .volume_mounts
            .as_ref()
            .unwrap()
            .iter()
            .filter(|m| m.read_only != Some(true))
            .map(|m| m.mount_path.as_str())
            .collect();
        writable.sort_unstable();
        assert_eq!(writable, vec!["/data", "/tmp"]);
    }

    #[test]
    fn user_namespaces_can_be_turned_off_for_a_cluster_that_lacks_them() {
        let mut c = test_config();
        c.user_namespaces = false;
        let d = tenant_deployment(&c);
        let pod = d.spec.unwrap().template.spec.unwrap();
        // Absent, not `true`: asking for the host's user namespace explicitly
        // would be a different and worse statement.
        assert_eq!(pod.host_users, None);
    }

    /// The v1 bug this contract calls out by name: the daemon reads these exact
    /// variable names, and nothing else configures it.
    #[test]
    fn the_daemon_gets_the_real_variable_names() {
        let c = test_config();
        let d = tenant_deployment(&c);
        let pod = d.spec.unwrap().template.spec.unwrap();
        let env = pod.containers[0].env.clone().unwrap();

        let plain: BTreeMap<&str, &str> = env
            .iter()
            .filter_map(|e| Some((e.name.as_str(), e.value.as_deref()?)))
            .collect();
        assert_eq!(plain["SQUELCH_BIND"], "0.0.0.0:8848");
        // Absent, the daemon serves no metrics at all; the collectors would
        // scrape a closed port forever and the dashboard would stay empty.
        assert_eq!(plain["SQUELCH_METRICS_BIND"], "0.0.0.0:9464");
        assert_eq!(plain["SQUELCH_CRED_BACKEND"], "file");
        assert_eq!(plain["SQUELCH_DB_PATH"], "/data/squelch.db");
        assert_eq!(plain["SQUELCH_CREDENTIALS_PATH"], "/data/credentials.json");
        assert_eq!(
            plain["SQUELCH_CRED_AGE_IDENTITY"],
            "/etc/squelch/identity/identity.txt"
        );
        assert_eq!(plain["HOME"], "/data");

        // The address comes from the Secret, so it is encrypted at rest and is
        // not in the Deployment anyone can `kubectl get -o yaml`.
        let email = env
            .iter()
            .find(|e| e.name == "SQUELCH_ACCOUNT_EMAIL")
            .unwrap();
        assert!(email.value.is_none());
        let from = email.value_from.clone().unwrap().secret_key_ref.unwrap();
        assert_eq!(from.name, "alice-identity");
        assert_eq!(from.key, "account-email");

        // With no gateway configured, the LLM block is entirely absent: not a
        // base URL, not a provider pin, not an optional key reference. This is
        // the byte-identical-when-unset half of the contract.
        // ANTHROPIC_API_KEY is in the list as a tombstone: the shared-key
        // bridge injected the real provider key under that name, and a revert
        // that resurrects it must fail here, not in production.
        for llm_var in [
            "SQUELCH_ANTHROPIC_BASE_URL",
            "SQUELCH_STAGE2_PROVIDER",
            "SQUELCH_STAGE2_API_KEY",
            "SQUELCH_ASSISTANT_API_KEY",
            "SQUELCH_STAGE1_MODEL",
            "SQUELCH_MODEL",
            "SQUELCH_STAGE1_GLOBAL_DAILY_CAP",
            "SQUELCH_STAGE2_GLOBAL_DAILY_CAP",
            "ANTHROPIC_API_KEY",
        ] {
            assert!(
                env.iter().all(|e| e.name != llm_var),
                "{llm_var} appears in an unconfigured pod"
            );
        }
    }

    /// The LLM half of the env contract: a configured gateway reaches the pod
    /// as a base URL, a provider pin, and an OPTIONAL secretKeyRef — a tenant
    /// whose virtual key was never minted must still boot, and the daemon
    /// fail-softs triage when the var is absent.
    #[test]
    fn a_configured_gateway_reaches_the_daemon_and_the_key_stays_optional() {
        let c = llm_config();
        let pod = tenant_deployment(&c).spec.unwrap().template.spec.unwrap();
        let env = pod.containers[0].env.clone().unwrap();

        let plain: BTreeMap<&str, &str> = env
            .iter()
            .filter_map(|e| Some((e.name.as_str(), e.value.as_deref()?)))
            .collect();
        assert_eq!(
            plain["SQUELCH_ANTHROPIC_BASE_URL"],
            "https://llm.passband.internal"
        );
        assert_eq!(plain["SQUELCH_STAGE2_PROVIDER"], "anthropic");

        let key = env
            .iter()
            .find(|e| e.name == "SQUELCH_STAGE2_API_KEY")
            .unwrap();
        // From the Secret, never inline in the pod spec.
        assert!(key.value.is_none());
        let from = key.value_from.clone().unwrap().secret_key_ref.unwrap();
        assert_eq!(from.name, "alice-llm");
        assert_eq!(from.key, "api-key");
        assert_eq!(from.optional, Some(true));

        // The assistant relay credential, same Secret, same optional shape: a
        // tenant minted before the assistant era has no such data key and its
        // pod must still boot.
        let assistant = env
            .iter()
            .find(|e| e.name == "SQUELCH_ASSISTANT_API_KEY")
            .unwrap();
        assert!(assistant.value.is_none());
        let from = assistant.value_from.clone().unwrap().secret_key_ref.unwrap();
        assert_eq!(from.name, "alice-llm");
        assert_eq!(from.key, "assistant-api-key");
        assert_eq!(from.optional, Some(true));

        // The base URL alone pins no model and no cap: those are separate
        // knobs, absent unless the operator set them.
        for tuned in [
            "SQUELCH_STAGE1_MODEL",
            "SQUELCH_MODEL",
            "SQUELCH_STAGE1_GLOBAL_DAILY_CAP",
            "SQUELCH_STAGE2_GLOBAL_DAILY_CAP",
        ] {
            assert!(env.iter().all(|e| e.name != tuned), "{tuned} appeared unset");
        }

        // "No tenant ever holds a real provider key" has to hold in BOTH
        // modes: a configured gateway injects the virtual key only, never the
        // shared-bridge ANTHROPIC_API_KEY this guarantee replaced.
        assert!(
            env.iter().all(|e| e.name != "ANTHROPIC_API_KEY"),
            "the raw provider key reappeared in a gateway-configured pod"
        );
    }

    /// The tuning knobs, each landing under the daemon's real variable name.
    /// Stage 2's model is `SQUELCH_MODEL`, not `SQUELCH_STAGE2_MODEL`: it
    /// predates the stage split.
    #[test]
    fn the_llm_tuning_knobs_land_under_the_daemons_real_names() {
        let mut c = llm_config();
        c.llm_stage1_model = Some("claude-haiku-4-5".to_string());
        c.llm_stage2_model = Some("claude-sonnet-4-5".to_string());
        c.llm_stage1_daily_cap = Some(200);
        c.llm_stage2_daily_cap = Some(40);
        let pod = tenant_deployment(&c).spec.unwrap().template.spec.unwrap();
        let env = pod.containers[0].env.clone().unwrap();

        let plain: BTreeMap<&str, &str> = env
            .iter()
            .filter_map(|e| Some((e.name.as_str(), e.value.as_deref()?)))
            .collect();
        assert_eq!(plain["SQUELCH_STAGE1_MODEL"], "claude-haiku-4-5");
        assert_eq!(plain["SQUELCH_MODEL"], "claude-sonnet-4-5");
        assert!(!plain.contains_key("SQUELCH_STAGE2_MODEL"));
        assert_eq!(plain["SQUELCH_STAGE1_GLOBAL_DAILY_CAP"], "200");
        assert_eq!(plain["SQUELCH_STAGE2_GLOBAL_DAILY_CAP"], "40");
    }

    /// The rotation mechanism for the virtual key, same story as the
    /// credential hash: a keyed pod carries the hash, an unkeyed pod carries
    /// nothing, and a different key is a different pod spec.
    #[test]
    fn a_new_llm_key_rolls_the_deployment_and_an_unkeyed_pod_carries_no_hash() {
        let c = llm_config();
        let n = name();
        let annotations = |llm_hash: Option<&str>| {
            deployment(&c, &n, &credential_hash("ct"), llm_hash)
                .spec
                .unwrap()
                .template
                .metadata
                .unwrap()
                .annotations
                .unwrap()
        };

        assert!(!annotations(None).contains_key(LLM_KEY_HASH_ANNOTATION));
        let first = annotations(Some(&credential_hash("key-one")));
        let a = first[LLM_KEY_HASH_ANNOTATION].clone();
        assert_eq!(
            a,
            annotations(Some(&credential_hash("key-one")))[LLM_KEY_HASH_ANNOTATION],
            "the same key is the same pod spec"
        );
        assert_ne!(
            a,
            annotations(Some(&credential_hash("key-two")))[LLM_KEY_HASH_ANNOTATION],
            "a new key must roll the pod"
        );
        // A hash, not the key: the annotation is world-readable.
        assert_eq!(a.len(), 64);
        assert!(a.bytes().all(|b| b.is_ascii_hexdigit()));
        // And the credential hash rides alongside untouched.
        assert!(first.contains_key(CREDENTIAL_HASH_ANNOTATION));
    }

    #[test]
    fn the_llm_secret_is_the_key_verbatim_under_the_one_data_key() {
        let c = test_config();
        let secret = llm_secret(&c, &name(), Some("sk-vk-abc123"), None);
        assert_eq!(secret.metadata.name.as_deref(), Some("alice-llm"));
        assert_eq!(secret.metadata.namespace.as_deref(), Some("tenants"));
        assert_eq!(secret.type_.as_deref(), Some("Opaque"));
        let data = secret.string_data.unwrap();
        assert_eq!(data[LLM_API_KEY_KEY], "sk-vk-abc123");
        assert_eq!(data.len(), 1);
    }

    /// The assistant key rides in the SAME Secret under its own data key, and
    /// only when one was minted: the no-assistant shape stays byte-identical
    /// to what the pre-assistant warden wrote.
    #[test]
    fn the_llm_secret_carries_the_assistant_key_only_when_minted() {
        let c = test_config();
        let secret = llm_secret(&c, &name(), Some("sk-vk-abc123"), Some("sk-vk-assistant"));
        let data = secret.string_data.unwrap();
        assert_eq!(data[LLM_API_KEY_KEY], "sk-vk-abc123");
        assert_eq!(data[ASSISTANT_API_KEY_KEY], "sk-vk-assistant");
        assert_eq!(data.len(), 2);

        // The other half-failed-mint shape: assistant alone, no triage entry.
        let secret = llm_secret(&c, &name(), None, Some("sk-vk-assistant"));
        let data = secret.string_data.unwrap();
        assert_eq!(data[ASSISTANT_API_KEY_KEY], "sk-vk-assistant");
        assert_eq!(data.len(), 1);
    }

    /// The combined roll hash: rotating EITHER key moves it, the same pair is
    /// the same hash, and the no-assistant shape is distinct from every keyed
    /// one. This is the single helper both provisioning sites go through.
    #[test]
    fn the_combined_llm_hash_moves_when_either_key_does() {
        let bare = llm_keys_hash(Some("triage-one"), None);
        assert_eq!(bare, llm_keys_hash(Some("triage-one"), None));
        assert_ne!(bare, llm_keys_hash(Some("triage-two"), None));

        let keyed = llm_keys_hash(Some("triage-one"), Some("assist-one"));
        assert_ne!(bare, keyed, "minting the assistant key must move the hash");
        assert_eq!(keyed, llm_keys_hash(Some("triage-one"), Some("assist-one")));
        assert_ne!(keyed, llm_keys_hash(Some("triage-one"), Some("assist-two")));
        assert_ne!(keyed, llm_keys_hash(Some("triage-two"), Some("assist-one")));

        // Either slot alone is its own shape, distinct from the pair.
        let assistant_only = llm_keys_hash(None, Some("assist-one"));
        assert_ne!(assistant_only, keyed);
        assert_ne!(assistant_only, llm_keys_hash(Some("assist-one"), None));

        // Still a bare hex digest: it lands in a world-readable annotation.
        assert_eq!(keyed.len(), 64);
        assert!(keyed.bytes().all(|b| b.is_ascii_hexdigit()));
    }

    /// Without these the daemon exits at boot: `Config::oauth_client` is
    /// required by `squelchd serve`, and a refresh token only works for the
    /// client that minted it, which for a hosted tenant is the control plane's
    /// web client.
    #[test]
    fn the_daemon_gets_the_oauth_client_the_control_plane_consented_with() {
        let c = test_config();
        let pod = tenant_deployment(&c).spec.unwrap().template.spec.unwrap();
        let env = pod.containers[0].env.clone().unwrap();

        for (name, key) in [
            ("SQUELCH_CLIENT_ID", "client_id"),
            ("SQUELCH_CLIENT_SECRET", "client_secret"),
        ] {
            let var = env.iter().find(|e| e.name == name).unwrap();
            // From the Secret, never inline: a client secret in a pod spec is a
            // client secret in `kubectl get deploy -o yaml`.
            assert!(var.value.is_none(), "{name} is inline in the pod spec");
            let from = var.value_from.clone().unwrap().secret_key_ref.unwrap();
            assert_eq!(from.name, "google-oauth-client");
            assert_eq!(from.key, key);
        }
    }

    #[test]
    fn the_oauth_secret_can_be_renamed() {
        let mut c = test_config();
        c.oauth_secret_name = "passband-oauth".to_string();
        let pod = tenant_deployment(&c).spec.unwrap().template.spec.unwrap();
        let refs: Vec<String> = pod.containers[0]
            .env
            .clone()
            .unwrap()
            .iter()
            .filter(|e| e.name.starts_with("SQUELCH_CLIENT_"))
            .map(|e| {
                e.value_from
                    .clone()
                    .unwrap()
                    .secret_key_ref
                    .unwrap()
                    .name
                    .clone()
            })
            .collect();
        assert_eq!(refs, vec!["passband-oauth", "passband-oauth"]);
    }

    /// The console's Google half is dead without this: the daemon renders the
    /// sign-in link only when `SQUELCH_CONSOLE_SSO_URL` is set, and nothing else
    /// in the hosted path sets it.
    #[test]
    fn the_console_sso_url_reaches_the_daemon_when_it_is_configured() {
        let mut c = test_config();
        c.console_sso_url = Some("https://signup.passband.app".to_string());
        let pod = tenant_deployment(&c).spec.unwrap().template.spec.unwrap();
        let var = pod.containers[0]
            .env
            .clone()
            .unwrap()
            .into_iter()
            .find(|e| e.name == "SQUELCH_CONSOLE_SSO_URL")
            .expect("SQUELCH_CONSOLE_SSO_URL is missing from the tenant pod");
        // Plain, not a secretKeyRef: it is an origin that ends up in a public
        // page, so there is nothing here to keep out of a pod spec.
        assert_eq!(var.value.as_deref(), Some("https://signup.passband.app"));
    }

    /// Unset means ABSENT, not empty. A self-host daemon's console is the
    /// pasted-code form alone, and a hosted deploy that has not configured the
    /// control plane's origin gets the same page rather than a broken link.
    #[test]
    fn no_console_sso_url_means_no_variable_at_all() {
        let c = test_config();
        assert!(c.console_sso_url.is_none());
        let pod = tenant_deployment(&c).spec.unwrap().template.spec.unwrap();
        assert!(
            !pod.containers[0]
                .env
                .clone()
                .unwrap()
                .iter()
                .any(|e| e.name == "SQUELCH_CONSOLE_SSO_URL")
        );
    }

    /// Every container bounded, and the one writable scratch volume bounded
    /// too. An unbounded tenant is every other tenant's problem.
    #[test]
    fn nothing_in_the_pod_is_unbounded() {
        let c = test_config();
        let pod = tenant_deployment(&c).spec.unwrap().template.spec.unwrap();

        let mut containers = pod.init_containers.clone().unwrap();
        containers.extend(pod.containers.clone());
        assert_eq!(containers.len(), 2);
        for container in &containers {
            let r = container.resources.clone().unwrap();
            let requests = r.requests.unwrap();
            let limits = r.limits.unwrap();
            for key in ["cpu", "memory", "ephemeral-storage"] {
                assert!(
                    requests.contains_key(key),
                    "{} has no {key} request",
                    container.name
                );
                assert!(
                    limits.contains_key(key),
                    "{} has no {key} limit",
                    container.name
                );
            }
        }

        // The daemon's numbers come from config, so an operator can size them
        // to the box.
        let daemon = pod.containers[0].resources.clone().unwrap();
        assert_eq!(
            daemon.requests.clone().unwrap()["cpu"],
            Quantity("100m".into())
        );
        assert_eq!(
            daemon.requests.clone().unwrap()["memory"],
            Quantity("256Mi".into())
        );
        assert_eq!(
            daemon.requests.unwrap()["ephemeral-storage"],
            Quantity("256Mi".into())
        );
        assert_eq!(
            daemon.limits.clone().unwrap()["cpu"],
            Quantity("1000m".into())
        );
        assert_eq!(
            daemon.limits.clone().unwrap()["memory"],
            Quantity("1Gi".into())
        );
        assert_eq!(
            daemon.limits.unwrap()["ephemeral-storage"],
            Quantity("1Gi".into())
        );

        // The one emptyDir. Without a sizeLimit it is the node's free space.
        let tmp = pod
            .volumes
            .as_ref()
            .unwrap()
            .iter()
            .find(|v| v.name == "tmp")
            .unwrap()
            .empty_dir
            .clone()
            .unwrap();
        assert_eq!(tmp.size_limit, Some(Quantity("512Mi".into())));
    }

    #[test]
    fn the_bounds_come_from_config() {
        let mut c = test_config();
        c.daemon_resources.memory_limit = "2Gi".to_string();
        c.tmp_size = "64Mi".to_string();
        let pod = tenant_deployment(&c).spec.unwrap().template.spec.unwrap();
        assert_eq!(
            pod.containers[0].resources.clone().unwrap().limits.unwrap()["memory"],
            Quantity("2Gi".into())
        );
        assert_eq!(
            pod.volumes
                .unwrap()
                .iter()
                .find(|v| v.name == "tmp")
                .unwrap()
                .empty_dir
                .clone()
                .unwrap()
                .size_limit,
            Some(Quantity("64Mi".into()))
        );
    }

    /// The re-consent path, asserted on the object: a different sealed blob is
    /// a different pod template, which is a roll, which re-runs the seeder.
    #[test]
    fn a_new_ciphertext_rolls_the_deployment() {
        let c = test_config();
        let n = name();
        let first = crate::testing::armored("first");
        let second = crate::testing::armored("second");

        let annotation = |ct: &str| {
            deployment(&c, &n, &credential_hash(ct), None)
                .spec
                .unwrap()
                .template
                .metadata
                .unwrap()
                .annotations
                .unwrap()[CREDENTIAL_HASH_ANNOTATION]
                .clone()
        };

        let a = annotation(&first);
        assert_eq!(a, annotation(&first), "the same blob is the same pod spec");
        assert_ne!(a, annotation(&second), "a new blob must roll the pod");
        // A hash, not the blob: this annotation is readable by anyone who can
        // read a Deployment.
        assert_eq!(a.len(), 64);
        assert!(a.bytes().all(|b| b.is_ascii_hexdigit()));
        assert!(!a.contains("AGE"));
    }

    /// The seeder's half of the same story. It is still a constant with no
    /// tenant value in it, and it now clobbers exactly when the mounted Secret
    /// differs from what it last installed.
    #[test]
    fn the_seed_script_reinstalls_only_a_changed_credential() {
        assert!(SEED_SCRIPT.contains("sha256sum /etc/squelch/credential/credentials.json"));
        assert!(SEED_SCRIPT.contains(CREDENTIAL_MARKER_PATH));
        assert!(SEED_SCRIPT.contains("if [ \"$want\" != \"$have\" ]"));
        // The old no-clobber guard is what made rotation impossible.
        assert!(!SEED_SCRIPT.contains("if [ ! -f /data/credentials.json ]"));
    }

    #[test]
    fn the_identity_is_projected_one_key_at_a_time_and_read_only() {
        let c = test_config();
        let d = tenant_deployment(&c);
        let pod = d.spec.unwrap().template.spec.unwrap();

        let identity = pod
            .volumes
            .as_ref()
            .unwrap()
            .iter()
            .find(|v| v.name == "identity")
            .unwrap()
            .secret
            .clone()
            .unwrap();
        assert_eq!(identity.secret_name.as_deref(), Some("alice-identity"));
        let items = identity.items.unwrap();
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].key, "identity.txt");
        assert_eq!(identity.default_mode, Some(0o400));

        let mount = pod.containers[0]
            .volume_mounts
            .as_ref()
            .unwrap()
            .iter()
            .find(|m| m.name == "identity")
            .unwrap();
        assert_eq!(mount.mount_path, "/etc/squelch/identity");
        assert_eq!(mount.read_only, Some(true));
    }

    /// The sealed blob is seeded onto the tenant's own volume, because the
    /// daemon rewrites it on every token refresh and a Secret mount is
    /// read-only. The daemon container must NOT mount the Secret.
    #[test]
    fn the_credential_secret_is_mounted_only_by_the_init_container() {
        let c = test_config();
        let d = tenant_deployment(&c);
        let pod = d.spec.unwrap().template.spec.unwrap();

        let daemon_mounts = pod.containers[0].volume_mounts.clone().unwrap();
        assert!(daemon_mounts.iter().all(|m| m.name != "credential"));

        let seed = &pod.init_containers.as_ref().unwrap()[0];
        assert_eq!(seed.name, "seed");
        let seed_mount = seed
            .volume_mounts
            .as_ref()
            .unwrap()
            .iter()
            .find(|m| m.name == "credential")
            .unwrap();
        assert_eq!(seed_mount.mount_path, "/etc/squelch/credential");
        assert_eq!(seed_mount.read_only, Some(true));
        // The script is a constant: no tenant value is interpolated into it.
        assert_eq!(seed.args.clone().unwrap(), vec![SEED_SCRIPT.to_string()]);
        assert!(!SEED_SCRIPT.contains("alice"));
    }

    #[test]
    fn the_model_volume_appears_only_when_one_is_configured() {
        let mut c = test_config();
        assert!(c.model_pvc.is_none());
        let pod = tenant_deployment(&c).spec.unwrap().template.spec.unwrap();
        assert!(pod.volumes.unwrap().iter().all(|v| v.name != "models"));

        c.model_pvc = Some("squelch-models".to_string());
        let pod = tenant_deployment(&c).spec.unwrap().template.spec.unwrap();
        let models = pod
            .volumes
            .as_ref()
            .unwrap()
            .iter()
            .find(|v| v.name == "models")
            .unwrap();
        assert_eq!(
            models.persistent_volume_claim.as_ref().unwrap().claim_name,
            "squelch-models"
        );
        assert_eq!(
            models.persistent_volume_claim.as_ref().unwrap().read_only,
            Some(true)
        );
        // Mounted by the seeder only; the daemon never sees it.
        assert!(
            pod.containers[0]
                .volume_mounts
                .as_ref()
                .unwrap()
                .iter()
                .all(|m| m.name != "models")
        );
    }

    #[test]
    fn a_rolling_update_can_never_double_mount_the_volume() {
        let c = test_config();
        let spec = tenant_deployment(&c).spec.unwrap();
        assert_eq!(spec.replicas, Some(1));
        assert_eq!(spec.strategy.unwrap().type_.as_deref(), Some("Recreate"));
    }

    /// The `/mcp` refusal, asserted on the typed object. Hosted publishes the
    /// human door and nothing else, so the agent door matches no rule and the
    /// ingress controller answers 404.
    #[test]
    fn the_ingress_does_not_route_the_agent_door() {
        let c = test_config();
        let ing = ingress(&c, &name());
        let spec = ing.spec.unwrap();
        assert_eq!(spec.ingress_class_name.as_deref(), Some("traefik"));

        let rules = spec.rules.unwrap();
        assert_eq!(rules.len(), 1);
        assert_eq!(rules[0].host.as_deref(), Some("alice.passband.email"));

        let paths = rules[0].http.clone().unwrap().paths;
        let declared: Vec<&str> = paths.iter().map(|p| p.path.as_deref().unwrap()).collect();
        // The human door is the API, the browser console, the pixel, and the
        // bare root's redirect. The agent door is not on this list and that is
        // the whole point.
        assert_eq!(declared, vec!["/client", "/console", "/t", "/"]);
        for path in &paths {
            // "/" is Exact and everything else is Prefix: an Exact root
            // matches one path in the universe, a Prefix root would match all
            // of them.
            let expected = if path.path.as_deref() == Some("/") { "Exact" } else { "Prefix" };
            assert_eq!(path.path_type, expected, "{:?}", path.path);
            assert_eq!(
                path.backend.service.as_ref().unwrap().name,
                "alice",
                "every path goes to the tenant's own Service"
            );
        }
        // The load-bearing assertion: nothing routes the agent door, under any
        // spelling. Prefix paths are checked as string prefixes (coarser than
        // Kubernetes' element-wise match, so passing here passes there); the
        // Exact root matches only a literal "/", which none of these are.
        let prefixes: Vec<&str> = paths
            .iter()
            .filter(|p| p.path_type == "Prefix")
            .map(|p| p.path.as_deref().unwrap())
            .collect();
        for spelling in ["/mcp", "/mcp/", "/mcp/messages", "/mcp/sse"] {
            assert!(
                !prefixes.iter().any(|p| spelling.starts_with(*p)),
                "{spelling} would be routed to the daemon"
            );
            assert_ne!(spelling, "/", "an Exact root cannot match {spelling}");
        }
        // ...and the new prefix reaches the console and stops there.
        assert!(declared.contains(&"/console"));
        for spelling in ["/console", "/console/callback", "/console/pair"] {
            assert!(
                declared.iter().any(|p| spelling.starts_with(*p)),
                "{spelling} would 404 at the ingress"
            );
        }
    }

    #[test]
    fn the_ingress_uses_the_wildcard_certificate() {
        let c = test_config();
        let tls = ingress(&c, &name()).spec.unwrap().tls.unwrap();
        assert_eq!(tls.len(), 1);
        assert_eq!(tls[0].secret_name.as_deref(), Some("passband-wildcard-tls"));
        assert_eq!(
            tls[0].hosts.clone().unwrap(),
            vec!["alice.passband.email".to_string()]
        );
    }

    /// The reachability claim: only the ingress controller gets in, only DNS
    /// and public HTTPS get out, and every cluster-internal range is excluded.
    #[test]
    fn the_network_policy_isolates_the_tenant() {
        let c = test_config();
        let spec = network_policy(&c, &name()).spec.unwrap();

        assert_eq!(
            spec.policy_types.unwrap(),
            vec!["Ingress".to_string(), "Egress".to_string()]
        );
        assert_eq!(
            spec.pod_selector.unwrap().match_labels.unwrap()["app.kubernetes.io/instance"],
            "alice"
        );

        let ingress_rules = spec.ingress.unwrap();
        assert_eq!(ingress_rules.len(), 2);
        let from = ingress_rules[0].from.clone().unwrap();
        assert_eq!(from.len(), 1);
        assert_eq!(
            from[0]
                .namespace_selector
                .clone()
                .unwrap()
                .match_labels
                .unwrap()["kubernetes.io/metadata.name"],
            "kube-system"
        );
        assert_eq!(
            from[0].pod_selector.clone().unwrap().match_labels.unwrap()["app.kubernetes.io/name"],
            "traefik"
        );
        assert_eq!(
            ingress_rules[0].ports.clone().unwrap()[0].port,
            Some(IntOrString::Int(8848))
        );

        let egress = spec.egress.unwrap();
        assert_eq!(egress.len(), 2);

        // DNS, and only CoreDNS.
        let dns_peer = egress[0].to.clone().unwrap();
        assert_eq!(
            dns_peer[0]
                .pod_selector
                .clone()
                .unwrap()
                .match_labels
                .unwrap()["k8s-app"],
            "kube-dns"
        );
        let dns_ports: Vec<_> = egress[0]
            .ports
            .clone()
            .unwrap()
            .into_iter()
            .map(|p| (p.protocol.unwrap(), p.port.unwrap()))
            .collect();
        assert_eq!(
            dns_ports,
            vec![
                ("UDP".to_string(), IntOrString::Int(53)),
                ("TCP".to_string(), IntOrString::Int(53)),
            ]
        );

        // The internet, minus everything in the cluster.
        let block = egress[1].to.clone().unwrap()[0].ip_block.clone().unwrap();
        assert_eq!(block.cidr, "0.0.0.0/0");
        let except = block.except.unwrap();
        for cidr in [
            "10.0.0.0/8",
            "172.16.0.0/12",
            "192.168.0.0/16",
            "169.254.0.0/16",
            "100.64.0.0/10",
        ] {
            assert!(except.contains(&cidr.to_string()), "{cidr} is reachable");
        }
        assert_eq!(
            egress[1].ports.clone().unwrap()[0].port,
            Some(IntOrString::Int(443))
        );
    }

    /// The metrics allowance, asserted as narrowly as it is written: one
    /// namespace AND one pod label, one port, and that port is not 8848.
    #[test]
    fn only_the_metrics_collector_reaches_the_metrics_port() {
        let c = test_config();
        let rule = network_policy(&c, &name()).spec.unwrap().ingress.unwrap()[1].clone();

        let from = rule.from.clone().unwrap();
        assert_eq!(from.len(), 1, "one peer, not a list of alternatives");
        // Both selectors on the SAME peer: a namespace selector alone would let
        // in anything that ever lands in `monitoring`.
        assert_eq!(
            from[0]
                .namespace_selector
                .clone()
                .unwrap()
                .match_labels
                .unwrap()["kubernetes.io/metadata.name"],
            "monitoring"
        );
        assert_eq!(
            from[0].pod_selector.clone().unwrap().match_labels.unwrap()["app.kubernetes.io/name"],
            "prometheus-agent"
        );
        assert!(from[0].ip_block.is_none());

        let ports = rule.ports.unwrap();
        assert_eq!(ports.len(), 1);
        assert_eq!(ports[0].port, Some(IntOrString::Int(9464)));
        assert_eq!(ports[0].protocol.as_deref(), Some("TCP"));
        // The load-bearing half: the human door stays sealed to Traefik, so a
        // collector that went rogue reads counters and not mail.
        assert_ne!(ports[0].port, Some(IntOrString::Int(DAEMON_PORT)));
    }

    /// The other half of the metrics isolation claim: the port exists on the
    /// pod and on nothing that publishes it. An Ingress can only reach a port
    /// its Service declares, so a Service without it cannot be routed to by any
    /// mistake in [`HUMAN_DOOR_PREFIXES`].
    #[test]
    fn the_metrics_port_is_on_the_pod_and_on_nothing_public() {
        let c = test_config();
        let n = name();

        let pod = tenant_deployment(&c).spec.unwrap().template.spec.unwrap();
        let ports = pod.containers[0].ports.clone().unwrap();
        let named: Vec<(&str, i32)> = ports
            .iter()
            .map(|p| (p.name.as_deref().unwrap(), p.container_port))
            .collect();
        assert_eq!(named, vec![("http", 8848), ("metrics", 9464)]);

        // Not on the Service...
        let service_ports = service(&c, &n).spec.unwrap().ports.unwrap();
        assert_eq!(service_ports.len(), 1);
        assert_eq!(service_ports[0].port, DAEMON_PORT);
        assert!(
            service_ports
                .iter()
                .all(|p| p.name.as_deref() != Some("metrics")),
            "the metrics port on the Service would make it routable"
        );

        // ...so nothing the Ingress declares can arrive at it, and no path
        // spelling reaches /metrics either.
        let rules = ingress(&c, &n).spec.unwrap().rules.unwrap();
        let paths = rules[0].http.clone().unwrap().paths;
        for path in &paths {
            assert_eq!(
                path.backend.service.as_ref().unwrap().port,
                Some(ServiceBackendPort {
                    name: Some("http".to_string()),
                    ..Default::default()
                })
            );
        }
        // Prefix paths are checked as string prefixes; the Exact root matches
        // only a literal "/", which /metrics is not.
        let prefixes: Vec<&str> = paths
            .iter()
            .filter(|p| p.path_type == "Prefix")
            .map(|p| p.path.as_deref().unwrap())
            .collect();
        for spelling in ["/metrics", "/metrics/"] {
            assert!(
                !prefixes.iter().any(|p| spelling.starts_with(*p)),
                "{spelling} would be routed to the daemon"
            );
            assert_ne!(spelling, "/", "an Exact root cannot match {spelling}");
        }
    }

    /// The kubelet probe hole: absent unless the operator states the node
    /// network, and when present it is one CIDR to one port and nothing else.
    #[test]
    fn the_node_may_probe_only_when_the_operator_says_so() {
        let mut c = test_config();
        assert!(c.node_cidr.is_none());
        // Traefik and the metrics collector; the node is not in the list yet.
        assert_eq!(
            network_policy(&c, &name())
                .spec
                .unwrap()
                .ingress
                .unwrap()
                .len(),
            2
        );

        c.node_cidr = Some("10.0.0.5/32".to_string());
        let ingress = network_policy(&c, &name()).spec.unwrap().ingress.unwrap();
        assert_eq!(ingress.len(), 3);
        let from = ingress[2].from.clone().unwrap();
        assert_eq!(from.len(), 1);
        let block = from[0].ip_block.clone().unwrap();
        assert_eq!(block.cidr, "10.0.0.5/32");
        assert!(block.except.is_none());
        // Still only the daemon port, and still nothing else about the peer:
        // node-originated traffic, not "anything on that address".
        assert!(from[0].pod_selector.is_none());
        assert!(from[0].namespace_selector.is_none());
        let ports = ingress[2].ports.clone().unwrap();
        assert_eq!(ports.len(), 1);
        assert_eq!(ports[0].port, Some(IntOrString::Int(8848)));
        assert_eq!(ports[0].protocol.as_deref(), Some("TCP"));
    }

    #[test]
    fn the_pvc_asks_the_configured_class_for_the_configured_size() {
        let c = test_config();
        let spec = data_pvc(&c, &name()).spec.unwrap();
        assert_eq!(
            spec.access_modes.unwrap(),
            vec!["ReadWriteOnce".to_string()]
        );
        assert_eq!(spec.storage_class_name.as_deref(), Some("local-path"));
        assert_eq!(
            spec.resources.unwrap().requests.unwrap()["storage"],
            Quantity("10Gi".to_string())
        );
    }

    #[test]
    fn the_service_targets_the_named_container_port() {
        let c = test_config();
        let spec = service(&c, &name()).spec.unwrap();
        assert_eq!(spec.type_.as_deref(), Some("ClusterIP"));
        assert_eq!(
            spec.selector.unwrap()["app.kubernetes.io/instance"],
            "alice"
        );
        let port = &spec.ports.unwrap()[0];
        assert_eq!(port.port, 8848);
        assert_eq!(
            port.target_port,
            Some(IntOrString::String("http".to_string()))
        );
    }

    #[test]
    fn the_pair_exec_carries_the_tenants_public_url() {
        let c = test_config();
        assert_eq!(
            pair_argv(&c, &name()),
            vec![
                "/usr/local/bin/squelchd".to_string(),
                "pair".to_string(),
                "--url".to_string(),
                "https://alice.passband.email".to_string(),
            ]
        );
    }

    #[test]
    fn the_pull_secret_appears_only_when_one_is_configured() {
        let mut c = test_config();
        let pod = tenant_deployment(&c).spec.unwrap().template.spec.unwrap();
        assert!(pod.image_pull_secrets.is_none());

        c.pull_secret = Some("ghcr".to_string());
        let pod = tenant_deployment(&c).spec.unwrap().template.spec.unwrap();
        assert_eq!(pod.image_pull_secrets.unwrap()[0].name, "ghcr");
    }
}
