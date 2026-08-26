#!/usr/bin/env bash
# Generate the RSA-2048 keypair used by aero-auth (RS256 JWT signing).
# Outputs PKCS#8 PEM to ./secrets/jwt_private.pem and SPKI PEM to ./secrets/jwt_public.pem.
#
# Run from repo root:
#     ./scripts/gen-jwt-keys.sh

set -euo pipefail

cd "$(dirname "$0")/.."

SECRETS_DIR="./secrets"
PRIV="${SECRETS_DIR}/jwt_private.pem"
PUB="${SECRETS_DIR}/jwt_public.pem"

mkdir -p "${SECRETS_DIR}"
chmod 700 "${SECRETS_DIR}"

if [[ -f "${PRIV}" && -f "${PUB}" ]]; then
    if [[ ! -s "${PRIV}" || ! -s "${PUB}" ]]; then
        echo "Existing JWT key file is empty in ${SECRETS_DIR}/" >&2
        exit 1
    fi
    chmod 600 "${PRIV}"
    chmod 644 "${PUB}"
    echo "JWT keys already present in ${SECRETS_DIR}/ (not overwritten)."
    exit 0
fi
if [[ -e "${PRIV}" || -e "${PUB}" ]]; then
    echo "Incomplete JWT keypair in ${SECRETS_DIR}/; refusing to overwrite either file." >&2
    echo "Move the surviving file aside or restore its matching key." >&2
    exit 1
fi

if ! command -v openssl >/dev/null 2>&1; then
    echo "openssl not found in PATH" >&2
    exit 1
fi

echo "Generating RSA-2048 private key -> ${PRIV}"
openssl genpkey \
    -algorithm RSA \
    -pkeyopt rsa_keygen_bits:2048 \
    -out "${PRIV}"
chmod 600 "${PRIV}"

echo "Deriving public key -> ${PUB}"
openssl pkey -in "${PRIV}" -traditional -out "${PRIV}.pkcs1" && mv "${PRIV}.pkcs1" "${PRIV}" && chmod 600 "${PRIV}"; openssl rsa -in "${PRIV}" -pubout -out "${PUB}" 2>/dev/null
chmod 644 "${PUB}"

echo
echo "Done. To wire into config.toml:"
echo "  jwt_private_key_pem = \"\"\""
echo "  \$(cat ${PRIV})"
echo "  \"\"\""
echo
echo "Or set env vars (escaping newlines as appropriate)."
