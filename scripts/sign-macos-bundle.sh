#!/bin/bash
# Sign the deployed VoiceHarnessClient.app with the iCloud entitlement so
# CKContainer.default() works and identity federation uses the iCloud key.
#
# All deployment-specific values come from the environment (never committed):
#   VH_SIGN_IDENTITY  codesigning identity name (security find-identity -v -p codesigning)
#   VH_TEAM_ID        10-char team id the identity belongs to
#   VH_BUNDLE_ID      the app bundle's bundle id
#   VH_CONTAINER_ID   iCloud container id (must match the iOS app when set)
#
# Prereqs:
#   - Apple Development signing identity (security find-identity -v -p codesigning)
#   - A macOS provisioning profile for the app containing the iCloud container
#     entitlement, generated in Xcode for VH_BUNDLE_ID; place it at
#     clients/macos/voice-harness-client/VoiceHarnessClient.provisionprofile
#     (Xcode: target → Signing & Capabilities → iCloud/CloudKit with
#     VH_CONTAINER_ID, then export the profile).
#   - The Entitlements.plist container id is synced to VH_CONTAINER_ID below.
#
# Usage:
#   VH_SIGN_IDENTITY="..." VH_TEAM_ID=... VH_BUNDLE_ID=... VH_CONTAINER_ID=... \
#     scripts/sign-macos-bundle.sh
set -euo pipefail

APP_DIR="${APP_DIR:-/Applications/VoiceHarnessClient.app}"
ENTITLEMENTS="$(cd "$(dirname "$0")/.." && pwd)/clients/macos/voice-harness-client/Entitlements.plist"
PROFILE="$(cd "$(dirname "$0")/.." && pwd)/clients/macos/voice-harness-client/VoiceHarnessClient.provisionprofile"

: "${VH_SIGN_IDENTITY:?set VH_SIGN_IDENTITY (security find-identity -v -p codesigning)}"
: "${VH_TEAM_ID:?set VH_TEAM_ID}"
: "${VH_BUNDLE_ID:?set VH_BUNDLE_ID}"
: "${VH_CONTAINER_ID:?set VH_CONTAINER_ID (must match the iOS app)}"

# Keep Entitlements.plist in sync with the environment's container id.
plutil -replace "com.apple.developer.icloud-container-identifiers.0" -string "$VH_CONTAINER_ID" "$ENTITLEMENTS"

# The app must not run while being re-signed.
pkill -f VoiceHarnessClient.app 2>/dev/null || true
sleep 1

# Embed the provisioning profile so the system (and SecTask) can see the
# entitlements granted to this bundle id.
if [[ -f "$PROFILE" ]]; then
  cp "$PROFILE" "$APP_DIR/Contents/embedded.provisionprofile"
else
  echo "NOTE: no provisioning profile at $PROFILE — skipping profile embed."
  echo "      CKContainer will still refuse to construct without the profile"
  echo "      registered with the team for $VH_BUNDLE_ID."
fi

codesign --force --sign "$VH_SIGN_IDENTITY" \
  --entitlements "$ENTITLEMENTS" \
  --options runtime \
  "$APP_DIR"

# Verify: signature valid + entitlements actually attached.
codesign --verify --deep --strict "$APP_DIR"
codesign -dv --entitlements :- "$APP_DIR" | grep -q "icloud-container-identifiers" || {
  echo "ERROR: iCloud entitlement missing after signing" >&2
  exit 1
}

echo "OK: signed with iCloud entitlement (team $VH_TEAM_ID)."
open "$APP_DIR"
