#!/usr/bin/env bash
set -euo pipefail

# Starts independent mail-server implementations on loopback-only ephemeral
# ports. Each generated/server certificate is passed only to cfg(test) hooks;
# the companion rejection test still exercises the public WebPKI-only path.

repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$repo_root"
server="${1:-all}"
build_root="${CARGO_TARGET_DIR:-$repo_root/target}"

case "$server" in
  greenmail|dovecot|all) ;;
  *)
    echo "Usage: $0 [greenmail|dovecot|all]" >&2
    exit 64
    ;;
esac

for required in docker nc cargo node openssl; do
  if ! command -v "$required" >/dev/null 2>&1; then
    echo "Required command is unavailable: $required" >&2
    exit 69
  fi
done
if ! docker info >/dev/null 2>&1; then
  echo "Docker daemon is unavailable; independent-server integration was not run." >&2
  exit 69
fi

run_root="$(mktemp -d "${TMPDIR:-/tmp}/dakia-mail-safety-servers.XXXXXX")"
containers=()
cleanup() {
  local exit_status=$?
  local container
  for container in "${containers[@]:-}"; do
    if [[ "$exit_status" -ne 0 ]]; then
      echo "--- $container logs after integration failure ---" >&2
      docker logs --tail 200 "$container" >&2 || true
    fi
    docker rm --force "$container" >/dev/null 2>&1 || true
  done
  rm -rf "$run_root"
  return "$exit_status"
}
trap cleanup EXIT

wait_for_port() {
  local port="$1"
  for _ in $(seq 1 30); do
    if nc -z 127.0.0.1 "$port" >/dev/null 2>&1; then
      return 0
    fi
    sleep 1
  done
  echo "Timed out waiting for loopback port $port" >&2
  return 1
}

# TCP readiness alone is insufficient for an implicit-TLS service: GreenMail
# can accept a socket while its TLS listener is still starting. This makes an
# unauthenticated loopback handshake before the public-root rejection test so
# an EOF cannot be mistaken for certificate validation.
tls_handshake() {
  local port="$1"
  local server_name="$2"
  node -e '
const tls = require("node:tls");
const [port, servername] = process.argv.slice(1);
let complete = false;
const finish = (status) => {
  if (complete) return;
  complete = true;
  socket.destroy();
  process.exit(status);
};
const socket = tls.connect({
  host: "127.0.0.1",
  port: Number(port),
  servername,
  rejectUnauthorized: false,
});
socket.setTimeout(3_000, () => finish(1));
socket.once("error", () => finish(1));
socket.once("secureConnect", () => {
  const leaf = socket.getPeerCertificate(true);
  if (!leaf || !leaf.raw || !leaf.subject || !leaf.subject.CN) return finish(1);
  finish(0);
});
' "$port" "$server_name"
}

wait_for_tls() {
  local port="$1"
  local server_name="$2"
  for _ in $(seq 1 30); do
    if tls_handshake "$port" "$server_name"; then
      return 0
    fi
    sleep 1
  done
  echo "Timed out waiting for loopback TLS handshake on port $port" >&2
  return 1
}

published_port() {
  local container="$1"
  local target_port="$2"
  docker port "$container" "$target_port/tcp" | sed -n '1s/.*://p'
}

assert_production_rejects_untrusted_tls() {
  local label="$1"
  local imap_port="$2"
  DAKIA_INDEPENDENT_SERVER_HOST=127.0.0.1 \
    DAKIA_INDEPENDENT_SERVER_PORT="$imap_port" \
    DAKIA_INDEPENDENT_SERVER_NAME=localhost \
    DAKIA_INDEPENDENT_USERNAME='synthetic@localhost' \
    DAKIA_INDEPENDENT_PASSWORD='synthetic-only-password' \
    cargo test --locked -p dakia-core --lib \
      independent_server_tests::external_server_rejects_untrusted_tls \
      -- --ignored --exact
  echo "$label: typed public-root TLS rejection passed."
}

run_greenmail() {
  local container="dakia-greenmail-${RANDOM}-${RANDOM}"
  local certificate_root="$run_root/greenmail-tls"
  mkdir -p "$certificate_root"
  # GreenMail's bundled certificate has a non-DNS common name. Generate an
  # ephemeral localhost leaf instead, so the production TLS stack can perform
  # both normal hostname validation and explicitly scoped test-CA validation.
  openssl req -x509 -newkey rsa:2048 -nodes -days 1 \
    -subj '/CN=Dakia GreenMail test CA' \
    -addext 'basicConstraints=critical,CA:true' \
    -addext 'keyUsage=critical,keyCertSign,cRLSign' \
    -keyout "$certificate_root/ca.key" \
    -out "$certificate_root/ca.crt" >/dev/null 2>&1
  openssl req -newkey rsa:2048 -nodes \
    -subj '/CN=localhost' \
    -keyout "$certificate_root/tls.key" \
    -out "$certificate_root/tls.csr" >/dev/null 2>&1
  printf '%s\n' \
    '[server]' \
    'basicConstraints=critical,CA:false' \
    'keyUsage=critical,digitalSignature,keyEncipherment' \
    'extendedKeyUsage=serverAuth' \
    'subjectAltName=DNS:localhost,IP:127.0.0.1' \
    > "$certificate_root/server.ext"
  openssl x509 -req -in "$certificate_root/tls.csr" \
    -CA "$certificate_root/ca.crt" -CAkey "$certificate_root/ca.key" -CAcreateserial \
    -days 1 -extfile "$certificate_root/server.ext" -extensions server \
    -out "$certificate_root/tls.crt" >/dev/null 2>&1
  openssl pkcs12 -export -name greenmail -out "$certificate_root/greenmail.p12" \
    -inkey "$certificate_root/tls.key" -in "$certificate_root/tls.crt" \
    -certfile "$certificate_root/ca.crt" -passout pass:synthetic-keystore-password >/dev/null 2>&1
  openssl x509 -in "$certificate_root/ca.crt" -outform DER -out "$certificate_root/tls.der"
  # GreenMail's generic IMAP APPEND requires the destination to exist. Its
  # supported preload layout creates this otherwise empty synthetic Sent
  # mailbox before Dakia connects.
  mkdir -p "$certificate_root/preload/synthetic@localhost/Sent"
  chmod 755 "$certificate_root"
  chmod 644 "$certificate_root/greenmail.p12" "$certificate_root/tls.der"
  # Exact image version, no floating latest tag. Docker publishes only to
  # loopback, while GreenMail itself binds inside the isolated container.
  docker pull greenmail/standalone:2.1.14 >/dev/null
  containers+=("$container")
  docker run -d --name "$container" \
    --memory=512m --pids-limit=200 \
    -e 'GREENMAIL_OPTS=-Dgreenmail.setup.test.all -Dgreenmail.hostname=0.0.0.0 -Dgreenmail.tls.keystore.file=/tls/greenmail.p12 -Dgreenmail.tls.keystore.password=synthetic-keystore-password -Dgreenmail.tls.key.password=synthetic-keystore-password -Dgreenmail.preload.dir=/tls/preload -Dgreenmail.users=synthetic:synthetic-only-password@localhost -Dgreenmail.users.login=email' \
    -v "$certificate_root:/tls:ro" \
    -p 127.0.0.1::3993 -p 127.0.0.1::3465 \
    greenmail/standalone:2.1.14 >/dev/null
  local imap_port smtp_port
  imap_port="$(published_port "$container" 3993)"
  smtp_port="$(published_port "$container" 3465)"
  wait_for_port "$imap_port"
  wait_for_port "$smtp_port"
  wait_for_tls "$imap_port" localhost
  wait_for_tls "$smtp_port" localhost
  # The production CLI must also reject this self-signed endpoint. Its output
  # is intentionally static; the ignored Rust test below validates the typed
  # certificate cause so a bad config or unreachable port cannot count as a
  # rejection pass.
  # An existing executable may predate the checked-out connector fixes. Cargo
  # verifies its inputs even on a warm build; executability is not provenance.
  cargo build --locked -p dakia-cli --bin dakia-provider-smoke
  local smoke_config smoke_stderr
  smoke_config="$(printf '{"version":1,"provider":"greenmail","accountEmail":"synthetic@localhost","imap":{"host":"127.0.0.1","port":%s,"security":"tls"},"smtp":{"host":"127.0.0.1","port":%s,"security":"tls"},"credentials":{"password":"synthetic-only-password"}}' "$imap_port" "$smtp_port")"
  smoke_stderr="$run_root/greenmail-provider-smoke.stderr"
  if PROVIDER_SMOKE_CONFIG="$smoke_config" "$build_root/debug/dakia-provider-smoke" 2>"$smoke_stderr"; then
    echo "Production provider-smoke accepted GreenMail's untrusted certificate." >&2
    return 1
  fi
  grep -Fx 'Provider smoke failed; no remote mailbox mutation was requested.' "$smoke_stderr" >/dev/null
  assert_production_rejects_untrusted_tls greenmail "$imap_port"
  DAKIA_INDEPENDENT_SERVER_HOST=127.0.0.1 \
    DAKIA_INDEPENDENT_SERVER_PORT="$imap_port" \
    DAKIA_INDEPENDENT_SERVER_NAME=localhost \
    DAKIA_INDEPENDENT_SMTP_PORT="$smtp_port" \
    DAKIA_INDEPENDENT_CA_DER_PATH="$certificate_root/tls.der" \
    DAKIA_INDEPENDENT_USERNAME='synthetic@localhost' \
    DAKIA_INDEPENDENT_PASSWORD='synthetic-only-password' \
    cargo test --locked -p dakia-core --lib \
      independent_server_tests::external_server_greenmail_roundtrip \
      -- --ignored --exact
  echo "greenmail: trusted cfg(test) IMAP/SMTP round trip passed."
}

run_dovecot() {
  local container="dakia-dovecot-${RANDOM}-${RANDOM}"
  local certificate_root="$run_root/dovecot-tls"
  local rawlog_root="$run_root/dovecot-rawlog"
  local rawlog_config="$run_root/dovecot-rawlog.conf"
  local fixture="$repo_root/crates/dakia-core/testdata/mime/greenmail-2.1.14-mixed-alternative-leaf-mime-contradiction.eml"
  local evidence_path="${DAKIA_DOVECOT_SELECTIVE_EVIDENCE_PATH:-$run_root/dovecot-selective-mime-evidence.json}"
  mkdir -p "$certificate_root" "$rawlog_root"
  chmod 777 "$rawlog_root"
  printf '%s\n' \
    'protocol imap {' \
    '  rawlog_dir = /rawlog' \
    '}' > "$rawlog_config"
  # The official 2.4 image reads tls.crt and tls.key from this path. This CA
  # exists only for this run and is supplied solely to the cfg(test) endpoint.
  openssl req -x509 -newkey rsa:2048 -nodes -days 1 \
    -subj '/CN=Dakia independent-server test CA' \
    -addext 'basicConstraints=critical,CA:true' \
    -addext 'keyUsage=critical,keyCertSign,cRLSign' \
    -keyout "$certificate_root/ca.key" \
    -out "$certificate_root/ca.crt" >/dev/null 2>&1
  openssl req -newkey rsa:2048 -nodes \
    -subj '/CN=localhost' \
    -keyout "$certificate_root/tls.key" \
    -out "$certificate_root/tls.csr" >/dev/null 2>&1
  printf '%s\n' \
    '[server]' \
    'basicConstraints=critical,CA:false' \
    'keyUsage=critical,digitalSignature,keyEncipherment' \
    'extendedKeyUsage=serverAuth' \
    'subjectAltName=DNS:localhost,IP:127.0.0.1' \
    > "$certificate_root/server.ext"
  openssl x509 -req -in "$certificate_root/tls.csr" \
    -CA "$certificate_root/ca.crt" -CAkey "$certificate_root/ca.key" -CAcreateserial \
    -days 1 -extfile "$certificate_root/server.ext" -extensions server \
    -out "$certificate_root/tls.crt" >/dev/null 2>&1
  openssl x509 -in "$certificate_root/ca.crt" -outform DER -out "$certificate_root/tls.der"
  # The official rootless image runs as its mail user. This one-run private
  # key is synthetic and deleted by the trap, so permit that user to read it.
  chmod 755 "$certificate_root"
  chmod 644 "$certificate_root/tls.crt" "$certificate_root/tls.key" "$certificate_root/tls.der"
  # Dovecot is an independent IMAP implementation. Its current rootless
  # image listens for implicit TLS IMAP on 31993. It does not provide SMTP,
  # so GreenMail retains coverage of the paired SMTP TLS rejection probe.
  docker pull dovecot/dovecot:2.4.5 >/dev/null
  containers+=("$container")
  docker run -d --name "$container" --memory=256m --pids-limit=200 \
    -e 'USER_PASSWORD={PLAIN}synthetic-only-password' \
    -v "$certificate_root:/etc/dovecot/ssl:ro" \
    -v "$rawlog_config:/etc/dovecot/conf.d/99-dakia-rawlog.conf:ro" \
    -v "$rawlog_root:/rawlog" \
    -v "$fixture:/fixtures/selective-mime.eml:ro" \
    -p 127.0.0.1::31993 dovecot/dovecot:2.4.5 >/dev/null
  local imap_port
  imap_port="$(published_port "$container" 31993)"
  wait_for_port "$imap_port"
  # The ignored core test below performs and verifies the actual TLS handshake
  # before authentication and sync. Avoid a separate `openssl s_client` probe:
  # Dovecot may keep that unauthenticated IMAP session open after stdin closes.
  DAKIA_INDEPENDENT_SERVER_HOST=127.0.0.1 \
    DAKIA_INDEPENDENT_SERVER_PORT="$imap_port" \
    DAKIA_INDEPENDENT_SERVER_NAME=localhost \
    DAKIA_INDEPENDENT_CA_DER_PATH="$certificate_root/tls.der" \
    DAKIA_INDEPENDENT_USERNAME='synthetic@localhost' \
    DAKIA_INDEPENDENT_PASSWORD='synthetic-only-password' \
    cargo test --locked -p dakia-core independent_server_tests::external_server_roundtrip \
      --lib -- --ignored --exact
  echo "dovecot: trusted cfg(test) production-connector round trip passed."

  # Seed the exact governed fictional fixture without relying on another mail
  # implementation. Only this dedicated one-run container is mutated.
  docker exec -i "$container" /dovecot/bin/doveadm save \
    -u 'synthetic@localhost' < "$fixture"
  DAKIA_INDEPENDENT_SERVER_HOST=127.0.0.1 \
    DAKIA_INDEPENDENT_SERVER_PORT="$imap_port" \
    DAKIA_INDEPENDENT_SERVER_NAME=localhost \
    DAKIA_INDEPENDENT_CA_DER_PATH="$certificate_root/tls.der" \
    DAKIA_INDEPENDENT_USERNAME='synthetic@localhost' \
    DAKIA_INDEPENDENT_PASSWORD='synthetic-only-password' \
    cargo test --locked -p dakia-core \
      independent_server_tests::external_dovecot_selective_mime_roundtrip \
      --lib -- --ignored --exact

  mkdir -p "$(dirname "$evidence_path")"
  DAKIA_DOVECOT_IMAGE_DIGEST="$(docker image inspect dovecot/dovecot:2.4.5 --format '{{index .RepoDigests 0}}')" \
    node scripts/verify-dovecot-selective-rawlog.mjs \
      "$rawlog_root" "$fixture" > "$evidence_path"
  cat "$evidence_path"
  echo "dovecot: production selective MIME round trip and raw-command audit passed."
}

if [[ "$server" == greenmail || "$server" == all ]]; then
  run_greenmail
fi
if [[ "$server" == dovecot || "$server" == all ]]; then
  run_dovecot
fi

echo "Independent-server harness completed; all containers will now be removed."
