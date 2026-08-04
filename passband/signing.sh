# Shared signing-identity lookup. Sourced by build.sh and build-release.sh;
# not meant to be executed directly.
#
# Echoes the name of a Developer ID Application identity from the keychain, or
# nothing when the machine has no such certificate. Callers decide what an
# empty result means: build.sh falls back to ad-hoc, build-release.sh errors.

detect_sign_identity() {
  security find-identity -v -p codesigning 2>/dev/null \
    | grep -m1 'Developer ID Application' \
    | sed -E 's/.*"(.*)"$/\1/'
}
