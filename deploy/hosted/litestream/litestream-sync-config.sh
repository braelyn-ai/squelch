#!/usr/bin/env bash
#
# litestream-sync-config.sh -- render /etc/litestream.yml from the tenant PVC
# directories that exist RIGHT NOW, and restart litestream if the render moved.
#
# WHY THIS EXISTS. Litestream's config lists explicit database paths, and
# tenants arrive whenever somebody finishes a signup. Nothing tells the host
# about a new tenant, so something has to look. This script is that something;
# litestream-config.timer runs it every two minutes.
#
# WHY NOT A REPLICA URL PER TENANT. Litestream 0.3 can take `url: s3://...`
# instead of the broken-out bucket/path/endpoint fields. We spell them out
# because the URL form and the field form are mutually exclusive (litestream
# errors on "cannot specify url & bucket"), and because the broken-out form is
# the one the restore drills in SETUP.md paste into a scratch config.
#
# Explicit generation, one `dbs:` entry per tenant, buys a stable and
# human-typeable backup key -- `tenants/<label>/store.db` -- with no Kubernetes
# PV uid baked into it. Delete and recreate a PVC and the uid changes; a key
# derived from the directory name would silently move and orphan the old
# backup. It also buys `meta-path` per database, which is how litestream's own
# bookkeeping stays OUT of a directory the tenant's pod can write to.
#
# TARGETS LITESTREAM 0.3.13, NOT 0.5.x. The two lines disagree about the schema
# in a way that fails SILENTLY: 0.3 wants `replicas:` (a list), 0.5 wants
# `replica:` (a single mapping). Feed 0.3.13 a 0.5-shaped config and it parses
# happily, reports the database, and attaches ZERO replicas -- a box that looks
# healthy and backs up nothing. Verified against the v0.3.13 binary, see the
# footer. We are on 0.3.13 on purpose: it is the last line with client-side age
# encryption, which 0.5 removed.
#
# WHAT IT WILL NOT DO. It never writes a credential or a key into the generated
# config: the config carries `${LITESTREAM_R2_*}` / `${LITESTREAM_AGE_*}` names
# and nothing else, and systemd loads the real values into litestream's
# environment from /etc/litestream/env. It refuses to run at all if that file
# is missing, not owned by root, or readable by anyone else.
#
# Safe to run by hand at any time. It is idempotent: no change to the tenant
# set means no write and no restart.
#
#   sudo /usr/local/bin/litestream-sync-config.sh          # normal
#   sudo VERBOSE=1 /usr/local/bin/litestream-sync-config.sh # say what it sees
#   sudo DRY_RUN=1 /usr/local/bin/litestream-sync-config.sh # print, change nothing
#
#   # A restore config: same tenants, plus the age identity reference that
#   # decryption needs and replication does not. Writes nothing itself.
#   umask 077
#   sudo DRY_RUN=1 WITH_IDENTITY=1 /usr/local/bin/litestream-sync-config.sh > /root/restore.yml
#
# See deploy/hosted/SETUP.md -> "Backups: Litestream to R2".

set -euo pipefail

# ---------------------------------------------------------------------------
# Parameters. Every one can be overridden from the environment, which is what
# makes this testable on a scratch box without editing the installed copy.
# ---------------------------------------------------------------------------

# Where k3s local-path carves tenant PVCs. This is the block-volume mount from
# SETUP.md ("Tenant data on a block volume"), i.e. the value of the k3s server
# flag --default-local-storage-path. If you did NOT repoint local-path, this is
# /var/lib/rancher/k3s/storage.
TENANT_ROOT="${TENANT_ROOT:-/mnt/tenant-data}"

# The Kubernetes namespace every tenant object lives in. local-path names each
# directory pvc-<pv-uid>_<namespace>_<pvc-name>, so this is half the glob.
K8S_NAMESPACE="${K8S_NAMESPACE:-tenants}"

# The daemon's SQLite file inside its volume. This is SQUELCH_DB_PATH from the
# tenant pod (/data/squelch.db) with the /data mount point stripped off; see
# `daemon_env` in squelch-warden/src/objects.rs.
DB_FILENAME="${DB_FILENAME:-squelch.db}"

# Object-storage prefix. A tenant's backup lands at <R2_PREFIX>/<label>/store.db.
R2_PREFIX="${R2_PREFIX:-tenants}"

# Credentials. Never read by this script, only named in the render.
ENV_FILE="${ENV_FILE:-/etc/litestream/env}"

# Litestream's default config path, and the one the shipped unit uses.
CONFIG_FILE="${CONFIG_FILE:-/etc/litestream.yml}"

# Where litestream keeps its per-database bookkeeping. Deliberately NOT next to
# the database (litestream's default is a hidden `.<name>-litestream` directory
# beside the file), because "next to the database" is inside a volume the
# tenant's own pod mounts read-write. `meta-path` is a real 0.3.13 config key,
# not a 0.5-ism -- see the footer for what that was checked against.
META_ROOT="${META_ROOT:-/var/lib/litestream}"

# The unit to bounce when the render changes.
SERVICE="${SERVICE:-litestream.service}"

# Emit the age IDENTITY reference as well as the recipient. Off for the config
# litestream replicates with -- replication needs only the public recipient, so
# the running daemon never holds the private key. Turn it on to render a config
# for `litestream restore`, which cannot decrypt without it.
WITH_IDENTITY="${WITH_IDENTITY:-0}"

# Every variable the rendered config expands. A typo'd or absent one expands to
# the empty string and litestream fails in a way that looks like an R2 outage,
# so check them here where the error can name the actual problem.
#
# LITESTREAM_AGE_RECIPIENT is in this list for a blunter reason: it is the
# difference between ciphertext and plaintext in somebody else's object store.
# Empty is not silently plaintext -- 0.3.13 refuses to start with "no recipients
# found" -- but a refusal at 3am reads as an outage, so catch it here instead.
REQUIRED_VARS=(
  LITESTREAM_R2_ENDPOINT
  LITESTREAM_R2_BUCKET
  LITESTREAM_R2_ACCESS_KEY_ID
  LITESTREAM_R2_SECRET_ACCESS_KEY
  LITESTREAM_AGE_RECIPIENT
)

VERBOSE="${VERBOSE:-0}"
DRY_RUN="${DRY_RUN:-0}"
FORCE="${FORCE:-0}"

# The age identity is deliberately NOT in REQUIRED_VARS and deliberately NOT in
# /etc/litestream/env. It lives in exactly one place on this box
# (/etc/litestream/backup-age.key, root:root 0600) plus your password manager,
# and the operator exports it into the restore shell for the minutes a restore
# takes. A second on-box copy is a second thing to rotate and a second thing to
# leak.

# ---------------------------------------------------------------------------
# Output. Goes to stderr so that `systemctl status` and `journalctl -u
# litestream-config` show it, and so stdout stays clean under DRY_RUN=1.
# ---------------------------------------------------------------------------

log() { printf 'litestream-sync-config: %s\n' "$*" >&2; }
vlog() { [[ "$VERBOSE" == "1" ]] && log "$@"; return 0; }
die() { printf 'litestream-sync-config: FATAL: %s\n' "$*" >&2; exit 1; }

# stat(1) is GNU on the target (Debian) and BSD on a macOS laptop. This runs on
# Debian; the fallback exists so the preflight can be exercised off-box without
# a second, drifting copy of this script.
#   stat_field u <file> -> owner uid
#   stat_field a <file> -> permission bits, octal
stat_field() {
  case "$1" in
    u) stat -c '%u' "$2" 2>/dev/null || stat -f '%u' "$2" ;;
    a) stat -c '%a' "$2" 2>/dev/null || stat -f '%Lp' "$2" ;;
  esac
}

# ---------------------------------------------------------------------------
# Preconditions. Each one is a way this has silently backed up nothing.
# ---------------------------------------------------------------------------

[[ "${EUID:-$(id -u)}" -eq 0 ]] ||
  die "must run as root: it writes ${CONFIG_FILE} and talks to systemd"

# A restore config is a thing you capture on stdout, never a thing you install.
# Installed at ${CONFIG_FILE} it would put the private key in the replicating
# daemon's environment, and it would do so invisibly.
if [[ "$WITH_IDENTITY" == "1" && "$DRY_RUN" != "1" ]]; then
  log "WITH_IDENTITY=1 renders a RESTORE config and must not overwrite ${CONFIG_FILE}."
  log "  umask 077"
  log "  DRY_RUN=1 WITH_IDENTITY=1 $0 > /root/restore.yml"
  die "WITH_IDENTITY=1 requires DRY_RUN=1"
fi

# The env file. This is the loud refusal the design asks for: a world-readable
# file here is every tenant's backup handed to anyone with a shell on the box.
[[ -e "$ENV_FILE" ]] ||
  die "${ENV_FILE} does not exist. Create it from deploy/hosted/litestream/env.example, chmod 0600, chown root:root."
[[ -f "$ENV_FILE" ]] ||
  die "${ENV_FILE} is not a regular file. A symlink or device here is somebody redirecting the credential."

env_owner="$(stat_field u "$ENV_FILE")"
env_mode="$(stat_field a "$ENV_FILE")"
[[ "$env_owner" == "0" ]] ||
  die "${ENV_FILE} is owned by uid ${env_owner}, not root. Run: chown root:root ${ENV_FILE}"
# Octal-mask the group and other bits. 8#$var forces base-8 on a value like 640.
if (( 8#$env_mode & 8#077 )); then
  die "${ENV_FILE} is mode ${env_mode} -- group or world readable. Run: chmod 0600 ${ENV_FILE}"
fi

# Each variable must be present AND non-empty. `grep` rather than `source`,
# deliberately: this script must never execute the contents of a credential
# file, and it has no business holding the secret in its own environment.
for var in "${REQUIRED_VARS[@]}"; do
  if ! grep -Eq "^[[:space:]]*${var}=[^[:space:]]" "$ENV_FILE"; then
    case "$var" in
      LITESTREAM_AGE_RECIPIENT)
        die "${ENV_FILE} has no non-empty ${var}. Litestream would expand it to \"\" and refuse to start with \"no recipients found\". Generate the keypair first: see SETUP.md 11 step 3." ;;
      *)
        die "${ENV_FILE} has no non-empty ${var}. Litestream would expand it to \"\" and fail as if R2 were down." ;;
    esac
  fi
done

# ONE canonical spelling of the endpoint: a bare host, no scheme. Litestream
# itself does not care (aws-sdk-go prepends https:// when a custom endpoint
# carries no scheme), but the runbooks compose "https://${LITESTREAM_R2_ENDPOINT}"
# for aws-cli and rclone, and "https://https://..." is a bad thing to debug
# during a restore. Refuse rather than rewrite: this is a root-owned credential
# file and silently editing it is worse than one clear sentence.
#
# This is the ONE value this script reads rather than merely tests for, and it
# is a public hostname. No key, secret or recipient is ever pulled out of the
# env file and into this process.
endpoint_value="$(sed -n 's/^[[:space:]]*LITESTREAM_R2_ENDPOINT=//p' "$ENV_FILE" | head -1 | tr -d "\"'")"
if [[ "$endpoint_value" == *://* ]]; then
  log "${ENV_FILE}: LITESTREAM_R2_ENDPOINT carries a URL scheme."
  log "  Canonical form is the bare host, e.g.:"
  log "    LITESTREAM_R2_ENDPOINT=0123456789abcdef0123456789abcdef.r2.cloudflarestorage.com"
  log "  Fix it with: sed -i 's|^LITESTREAM_R2_ENDPOINT=.*://|LITESTREAM_R2_ENDPOINT=|' ${ENV_FILE}"
  die "endpoint must not include a scheme"
fi

# The tenant root. If the block volume failed to mount, this directory either
# does not exist or is an empty stub on the root disk -- and the empty stub is
# the dangerous one, because it renders a config with zero databases and looks
# like a box that simply has no tenants yet. The FORCE gate below is the guard.
[[ -d "$TENANT_ROOT" ]] ||
  die "tenant root ${TENANT_ROOT} does not exist. Is the block volume mounted? Check: findmnt ${TENANT_ROOT}"

# ---------------------------------------------------------------------------
# Discovery. One line per tenant, "<label>\t<db path>", sorted, so the render
# is byte-stable and the diff below means what it says.
# ---------------------------------------------------------------------------

# nullglob so a root with no tenants yields nothing rather than the literal
# glob string. Scoped to this script; it is not exported anywhere.
shopt -s nullglob

discovered=""
skipped=0

for dir in "$TENANT_ROOT"/pvc-*_"${K8S_NAMESPACE}"_*-data; do
  [[ -d "$dir" ]] || continue

  db="${dir}/${DB_FILENAME}"
  if [[ ! -f "$db" ]]; then
    # Normal and transient: the PVC binds when the pod first schedules, and the
    # daemon creates its SQLite a moment later. It will be here next tick.
    vlog "skip (no ${DB_FILENAME} yet): ${dir}"
    skipped=$((skipped + 1))
    continue
  fi

  base="${dir##*/}"        # pvc-<pv-uid>_<namespace>_<label>-data
  pvc_name="${base##*_}"   # <label>-data   (labels and namespaces have no "_")
  label="${pvc_name%-data}"

  # Re-validate the label against the warden's own rule (squelch-warden's
  # validate.rs: 3-30 chars of a-z0-9 and hyphen, no leading or trailing
  # hyphen). This directory name is not attacker-controlled, but the label goes
  # straight into an object-storage key and into a YAML document, and "derived
  # from a filename" is exactly how that stops being true one refactor later.
  if [[ ! "$label" =~ ^[a-z0-9][a-z0-9-]{1,28}[a-z0-9]$ ]]; then
    log "REFUSING directory with an implausible tenant label: ${dir}"
    log "  parsed label: '${label}' -- expected 3-30 chars of a-z 0-9 and hyphen."
    die "unparseable tenant directory; not writing a config I do not understand"
  fi

  discovered+="${label}"$'\t'"${db}"$'\n'
done

# Sort by label so two runs over the same tenant set render identical bytes.
if [[ -n "$discovered" ]]; then
  discovered="$(printf '%s' "$discovered" | LC_ALL=C sort)"
  discovered+=$'\n'
fi

# Two directories claiming one label means two databases racing for one backup
# key, which is a corrupted backup rather than a duplicated one. The usual
# cause is a PVC that was deleted and recreated, leaving the old uid's
# directory behind. Refuse; a human has to say which one is the mailbox.
if [[ -n "$discovered" ]]; then
  dupes="$(printf '%s' "$discovered" | cut -f1 | LC_ALL=C uniq -d || true)"
  if [[ -n "$dupes" ]]; then
    log "two PVC directories claim the same tenant label(s):"
    printf '%s\n' "$dupes" >&2
    log "Both would replicate to ${R2_PREFIX}/<label>/store.db and corrupt each other."
    log "Find them with: ls -d ${TENANT_ROOT}/pvc-*_${K8S_NAMESPACE}_<label>-data"
    log "Then move the stale one aside (do NOT delete until you are sure)."
    die "ambiguous tenant set; refusing to render"
  fi
fi

tenant_count=0
[[ -n "$discovered" ]] && tenant_count="$(printf '%s' "$discovered" | grep -c '' || true)"

# The volume-not-mounted guard. Going from N tenants to zero is never something
# that happens on its own: tenants are deleted one at a time and by a human.
# Zero-from-many is a missing mount, and rewriting the config would stop
# backing up every tenant on the box while looking like a successful run.
if [[ "$tenant_count" -eq 0 && -f "$CONFIG_FILE" ]]; then
  previous="$(grep -c '^  - path:' "$CONFIG_FILE" || true)"
  if [[ "${previous:-0}" -gt 0 && "$FORCE" != "1" ]]; then
    log "found 0 tenants but ${CONFIG_FILE} currently replicates ${previous}."
    log "That is a missing mount, not an empty box. Check: findmnt ${TENANT_ROOT}"
    log "If every tenant really was removed on purpose, re-run with FORCE=1."
    die "refusing to erase a working config"
  fi
fi

# ---------------------------------------------------------------------------
# Render.
# ---------------------------------------------------------------------------

# mktemp beside the target so the install below is a rename(2) within one
# filesystem: litestream never sees a half-written config.
tmp="$(mktemp "${CONFIG_FILE}.XXXXXX")"
trap 'rm -f "$tmp"' EXIT
chmod 0644 "$tmp"

{
  cat <<'HEADER'
# GENERATED FILE -- DO NOT EDIT.
#
# Rendered by /usr/local/bin/litestream-sync-config.sh, which
# litestream-config.timer runs every two minutes. Any edit here survives
# exactly until the next tick. Change the script, or /etc/litestream/env.
#
# Schema is LITESTREAM 0.3.x: `replicas:` is a LIST, and `age:` sits on the
# replica. 0.5.x renamed it to a single `replica:` and deleted age entirely, so
# do not "modernise" this file -- 0.3.13 accepts a 0.5-shaped config without
# complaint and attaches no replicas at all.
#
# There are no credentials and no key material in this file. Litestream expands
# ${VAR} from its own process environment, and systemd loads that environment
# from /etc/litestream/env (root:root, 0600). That is the entire reason this
# file is boring enough to paste into a ticket.
#
# LITESTREAM_AGE_RECIPIENT is the PUBLIC half of the backup keypair: this
# process can seal a backup and cannot open one. Restoring needs the identity,
# which is not here and not in /etc/litestream/env -- see README.md.
#
# One litestream process on the HOST replicates every tenant. Not a sidecar in
# each tenant pod, on purpose: a sidecar needs the R2 write credential inside
# the pod, and one compromised tenant could then delete every other tenant's
# backups. Root already owns this disk. See deploy/hosted/litestream/README.md.
HEADER

  if [[ "$WITH_IDENTITY" == "1" ]]; then
    cat <<'IDENTITY_HEADER'
#
# RENDERED WITH WITH_IDENTITY=1: this is a RESTORE config, not the one
# litestream replicates with. It references ${LITESTREAM_AGE_IDENTITY}, which
# the restoring shell must export from /etc/litestream/backup-age.key. The key
# itself is still not in this file; an unset variable fails loudly with
# "no secret keys found" rather than handing you an unreadable download.
IDENTITY_HEADER
  fi

  printf '#\n# Rendered for %d tenant database(s).\n\n' "$tenant_count"

  cat <<'GLOBALS'
logging:
  level: info
  type: text

GLOBALS

  if [[ "$tenant_count" -eq 0 ]]; then
    cat <<'EMPTY'
# No tenant databases on this box yet. Litestream logs "no databases specified
# in configuration" -- at ERROR level, which is alarming and harmless -- and
# then idles. It does not exit, so the unit stays up and picks tenants up as
# the timer finds them.
dbs: []
EMPTY
  else
    printf 'dbs:\n'
    while IFS=$'\t' read -r label db; do
      [[ -n "$label" ]] || continue
      cat <<ENTRY
  - path: ${db}
    # Litestream's bookkeeping, kept off the tenant's own volume: the default
    # would be a .${DB_FILENAME}-litestream directory inside a mount the tenant
    # pod writes to.
    meta-path: ${META_ROOT}/${label}
    # A LIST, because this is the 0.3 schema. Exactly one entry.
    replicas:
      - type: s3
        bucket: \${LITESTREAM_R2_BUCKET}
        path: ${R2_PREFIX}/${label}/store.db
        # Bare host, no scheme; the preflight refuses one with a scheme. An
        # explicit endpoint also switches litestream to path-style addressing
        # on its own, which is what R2 wants -- no force-path-style needed.
        endpoint: \${LITESTREAM_R2_ENDPOINT}
        region: auto
        access-key-id: \${LITESTREAM_R2_ACCESS_KEY_ID}
        secret-access-key: \${LITESTREAM_R2_SECRET_ACCESS_KEY}
        # Client-side encryption. Everything above this line describes where the
        # bytes go; this is why Cloudflare cannot read them.
        age:
ENTRY
      if [[ "$WITH_IDENTITY" == "1" ]]; then
        cat <<'IDENTITY_ENTRY'
          identities:
            - ${LITESTREAM_AGE_IDENTITY}
IDENTITY_ENTRY
      fi
      cat <<'RECIPIENT_ENTRY'
          recipients:
            - ${LITESTREAM_AGE_RECIPIENT}
RECIPIENT_ENTRY
    done <<< "$discovered"
  fi
} > "$tmp"

if [[ "$DRY_RUN" == "1" ]]; then
  log "DRY_RUN=1: ${tenant_count} tenant(s), ${skipped} directory(ies) without a database yet"
  if [[ "$WITH_IDENTITY" == "1" ]]; then
    log "WITH_IDENTITY=1: restore config. Before using it, in the SAME shell:"
    log "  export LITESTREAM_AGE_IDENTITY=\"\$(grep -v '^#' /etc/litestream/backup-age.key | tr -d '[:space:]')\""
  fi
  cat "$tmp"
  exit 0
fi

# ---------------------------------------------------------------------------
# Install, but only if it actually moved. The restart below interrupts
# replication for EVERY tenant for a second or two, so it is worth not doing.
# ---------------------------------------------------------------------------

if [[ -f "$CONFIG_FILE" ]] && cmp -s "$tmp" "$CONFIG_FILE"; then
  vlog "no change (${tenant_count} tenant(s), ${skipped} not ready)"
  exit 0
fi

# Metadata directories, before litestream is told to look for them. 0700
# because the transaction state describes what is in the backup.
if [[ -n "$discovered" ]]; then
  while IFS=$'\t' read -r label _db; do
    [[ -n "$label" ]] || continue
    install -d -m 0700 -o root -g root "${META_ROOT}/${label}"
  done <<< "$discovered"
fi

mv -f "$tmp" "$CONFIG_FILE"
trap - EXIT
chown root:root "$CONFIG_FILE"
chmod 0644 "$CONFIG_FILE"

log "wrote ${CONFIG_FILE}: ${tenant_count} tenant database(s)"

# `restart` and not `reload`: litestream has no reload signal, and `restart`
# also STARTS the unit if it was stopped, which is what makes the very first
# tenant on a fresh box bring replication up on its own.
if systemctl restart "$SERVICE"; then
  log "restarted ${SERVICE}"
else
  # Do not exit non-zero into a crash loop of the timer. The config is correct
  # and on disk; the next tick will try again, and the journal has the reason.
  log "WARNING: systemctl restart ${SERVICE} failed. Check: systemctl status ${SERVICE}"
fi

# ---------------------------------------------------------------------------
# Verified against litestream v0.3.13 on 2026-08-11:
#
#   - `bash -n` clean.
#   - The render was produced by this script against a scratch tenant layout
#     (pvc-<uid>_tenants_<label>-data/squelch.db) and accepted by the real
#     v0.3.13 binary: `litestream databases -config <render>` lists every
#     tenant with an `s3` replica, with LITESTREAM_R2_* and
#     LITESTREAM_AGE_RECIPIENT exported. That command constructs the replica
#     and parses the age recipient, so it is a schema check, not a syntax one.
#   - The same render with `replicas:` collapsed to 0.5's `replica:` parses
#     with NO error and NO replicas. That is why the comment above shouts.
#   - meta-path: DBConfig.MetaPath -> db.SetMetaPath in v0.3.13
#     cmd/litestream/main.go, and accepted by the binary.
#   - age lives on the replica as `age: {identities, recipients}`:
#     v0.3.13 cmd/litestream/main.go ReplicaConfig.
#   - Empty LITESTREAM_AGE_RECIPIENT is a hard failure ("no recipients found"),
#     not silent plaintext. Empty LITESTREAM_AGE_IDENTITY is likewise
#     "no secret keys found". Both observed, not assumed.
#
# systemd-analyze verify was NOT run: this laptop is macOS and has no systemd.
# The units in this directory are unverified in that specific sense.
# ---------------------------------------------------------------------------
