#!/usr/bin/env bash
# Strip the mechanical flutter_rust_bridge scaffolding from a vendored api.rs.
# Run on a COPY (e.g. src/api/mod.rs). Manual cleanup is still required for the
# #[frb(mirror(..))] items — those redeclare rustpush types and must be deleted
# whole, not just de-annotated.
set -euo pipefail
f="${1:?usage: de_frb.sh path/to/api.rs}"

# 1. remove single-line #[frb(...)] attributes
sed -i -E '/^[[:space:]]*#\[frb\([^]]*\)\][[:space:]]*$/d' "$f"
# 2. drop flutter_rust_bridge imports and frb_generated references
sed -i -E '/use[[:space:]]+flutter_rust_bridge/d; /frb_generated/d' "$f"
# 3. drop uniffi scaffolding if present
sed -i -E '/uniffi::setup_scaffolding!\(\);/d' "$f"

echo "stripped simple #[frb(..)] / frb_generated / uniffi from $f"
echo
echo "STILL TO DO BY HAND:"
echo "  * delete every #[frb(mirror(..))] item (struct/enum) — types come from rustpush:"
grep -n 'frb(mirror' "$f" || echo "    (none found)"
echo "  * replace DartFnFuture<..> params (e.g. get_entitlements) with a plain"
echo "    async closure, or drop the fn — not on the onboarding path:"
grep -n 'DartFnFuture' "$f" || echo "    (none found)"
