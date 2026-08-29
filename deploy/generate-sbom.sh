#!/usr/bin/env bash
set -euo pipefail

# Emit a deterministic SPDX 2.3 inventory from Cargo's locked resolve graph.
# This is intentionally a build-time helper: ASP binaries do not parse or
# trust the SBOM at runtime, and signing/attestation remain release-system
# responsibilities.

usage() {
  echo "usage: $0 OUTPUT.spdx.json [VERSION] [TARGET]" >&2
  exit 2
}

[[ $# -ge 1 && $# -le 3 ]] || usage
output=$1
version=${2:-workspace}
target=${3:-unknown}

command -v cargo >/dev/null 2>&1 || {
  echo "generate-sbom.sh requires cargo" >&2
  exit 2
}
command -v jq >/dev/null 2>&1 || {
  echo "generate-sbom.sh requires jq" >&2
  exit 2
}

repo_root=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)
output_parent=$(dirname -- "$output")
mkdir -p -- "$output_parent"
temporary="$output.tmp.$$"
if [[ -e "$temporary" || -L "$temporary" ]]; then
  echo "temporary SBOM path already exists: $temporary" >&2
  exit 1
fi
cleanup() {
  rm -f -- "$temporary"
}
trap cleanup EXIT INT TERM

(cd "$repo_root" && cargo metadata --locked --format-version 1) |
  jq -cS --arg version "$version" --arg target "$target" '
    def safe($value): ($value | gsub("[^A-Za-z0-9.-]"; "-"));
    def package_id($index; $package):
      "SPDXRef-Package-\($index)-\(safe($package.name))-\(safe($package.version))";

    (.packages | sort_by([.name, .version, (.source // "")])) as $packages
    | ($packages
       | to_entries
       | map({key: .value.id, value: package_id(.key; .value)})
       | from_entries) as $ids
    | ($packages
       | to_entries
       | map({
           SPDXID: package_id(.key; .value),
           name: .value.name,
           versionInfo: .value.version,
           downloadLocation: (.value.source // "NOASSERTION"),
           filesAnalyzed: false,
           licenseConcluded: "NOASSERTION",
           licenseDeclared: (.value.license // "NOASSERTION"),
           supplier: "NOASSERTION",
           externalRefs: [
             {
               referenceCategory: "PACKAGE-MANAGER",
               referenceType: "purl",
               referenceLocator: ("pkg:cargo/" + .value.name + "@" + .value.version)
             }
           ]
         })) as $spdx_packages
    | ([
         {spdxElementId: "SPDXRef-DOCUMENT", relationshipType: "DESCRIBES", relatedSpdxElement: $spdx_packages[].SPDXID}
       ]
       + [
         (.resolve.nodes // [])[] as $node
         | select($ids[$node.id] != null)
         | ($node.dependencies // [])[] as $dependency
         | select($ids[$dependency] != null)
         | {
             spdxElementId: $ids[$node.id],
             relationshipType: "DEPENDS_ON",
             relatedSpdxElement: $ids[$dependency]
           }
       ]
       | unique_by([.spdxElementId, .relationshipType, .relatedSpdxElement])
       | sort_by([.spdxElementId, .relationshipType, .relatedSpdxElement])) as $relationships
    | {
        SPDXID: "SPDXRef-DOCUMENT",
        spdxVersion: "SPDX-2.3",
        dataLicense: "CC0-1.0",
        name: ("asp-" + $version + "-" + $target),
        documentNamespace: ("https://asp.invalid/sbom/" + safe($version) + "/" + safe($target)),
        creationInfo: {
          created: "1970-01-01T00:00:00Z",
          creators: ["Tool: ASP deterministic SBOM generator"]
        },
        packages: $spdx_packages,
        relationships: $relationships
      }
  ' >"$temporary"

chmod 0644 "$temporary"
mv -- "$temporary" "$output"
trap - EXIT INT TERM
printf 'ASP SBOM written to %s\n' "$output"
