#!/usr/bin/env bash
# Docs check (CI runs it): markdown only at the allowed paths, CLAUDE.md a single import,
# no private data in any file, no em dashes in any markdown, no maturity hedges, history or
# test provenance in the user docs, and each page under its line budget. Prints one
# `path:line: rule: excerpt` line per violation, then a count, and exits 1 when there is
# any. Untracked files that are not ignored are checked too, so an internal file fails here
# before it is committed. Needs bash, git and a POSIX awk only.
set -euo pipefail
cd "$(git rev-parse --show-toplevel)"
export LC_ALL=C

# Git with paths printed as they are: a quoted non-ASCII name would match no rule.
git() { command git -c core.quotePath=false "$@"; }

violations=0

# report PATH LINE RULE EXCERPT
report() {
  local excerpt=$4
  excerpt=${excerpt#"${excerpt%%[![:space:]]*}"}
  printf '%s:%s: %s: %s\n' "$1" "$2" "$3" "${excerpt:0:120}"
  violations=$((violations + 1))
}

# Tracked files and untracked ones that are not ignored, each once, as long as they exist
# on disk. File lists are NUL-separated, so every name is read whole.
declare -A seen=()
files=()
while IFS= read -r -d '' f; do
  [[ -f $f && -z ${seen[$f]+x} ]] || continue
  seen[$f]=1
  files+=("$f")
done < <(git ls-files -z --cached --others --exclude-standard)

# 1. Markdown lives only at these paths. Specs, plans, notes and other internal files go
#    under .claude/ (ignored), never in the repo.
allowed='^(README|CONTRIBUTING|CHANGELOG|AGENTS|CLAUDE)\.md$'
allowed+='|^docs/(architecture|cli|comparison|compatibility|faq|integration-guide|protocol|tree-guide)\.md$'
allowed+='|^bench/(README|RESULTS)\.md$|^bench/dual-demo/MAPPING\.md$|^demo/README\.md$'
allowed+='|^\.github/([^/]+/)*[^/]+\.md$'
markdown=()
for f in "${files[@]}"; do
  [[ ${f,,} == *.md || ${f,,} == *.markdown ]] || continue
  markdown+=("$f")
  [[ $f =~ $allowed ]] || report "$f" 1 allowlist "not an allowed docs path; internal files go under .claude/"
done

# 2. CLAUDE.md is exactly `@AGENTS.md` (a trailing newline allowed): the guide is AGENTS.md.
if [[ -n ${seen[CLAUDE.md]+x} ]]; then
  while IFS= read -r hit; do
    report CLAUDE.md "${hit%%:*}" claude-md "${hit#*:}"
  done < <(awk '
    NR == 1 && $0 == "@AGENTS.md" { next }
    { print NR ":" ($0 == "" ? "blank line; the file is exactly @AGENTS.md" : $0) }
    END { if (NR == 0) print "1:empty; the file is exactly @AGENTS.md" }
  ' CLAUDE.md)
  [[ -n ${seen[AGENTS.md]+x} ]] || report CLAUDE.md 1 claude-md "imports AGENTS.md, which is missing"
fi

# 3. Private data, in every text file but Cargo.lock. git grep -z prints
#    `path NUL line NUL text` records; binary files are skipped (-I).
not_lock=(':(exclude)Cargo.lock' ':(exclude)*/Cargo.lock')
# grep_all GIT-GREP-OPTIONS...
grep_all() { git grep -z -nI --untracked "$@" -- "${not_lock[@]}"; }

# Home directories, other than the placeholder users the fixtures and tests use.
while IFS= read -r -d '' path && IFS= read -r -d '' line && IFS= read -r m; do
  name=${m#/home/}
  while [[ $name == *. ]]; do name=${name%.}; done
  [[ $name == user || $name == someone ]] || report "$path" "$line" private-path "$m"
done < <(grep_all -oE '/home/[A-Za-z_][A-Za-z0-9_.-]*')
# The maintainer's checkouts (the class keeps this line from matching itself).
while IFS= read -r -d '' path && IFS= read -r -d '' line && IFS= read -r m; do
  report "$path" "$line" private-path "$m"
done < <(grep_all -oE '~/Project[s]/[^[:space:]`"'\'')]*')
# Session links, other than the bench fixtures' placeholders. The dot stays escaped, so
# this script holds no link itself.
while IFS= read -r -d '' path && IFS= read -r -d '' line && IFS= read -r m; do
  case ${m#*/code/} in
  session_FIXTURE | session_OTHER | 'session_<id>') ;;
  *) report "$path" "$line" private-link "$m" ;;
  esac
done < <(grep_all -oE 'claude\.ai/(code/session|chat/|share/)[A-Za-z0-9_<>-]*')
# Email addresses. Allowed: the authors line of the root Cargo.toml (the maintainer's public
# crates.io address), Claude's and the Actions bot's noreply addresses, and the reserved
# example domains.
email_re='[][A-Za-z0-9._%+-]+@[A-Za-z0-9-]+(\.[A-Za-z0-9-]+)*\.[A-Za-z]{2,}'
email_ok='^noreply@anthropic\.com$|^41898282\+github-actions\[bot\]@users\.noreply\.github\.com$'
email_ok+='|@example\.(com|org|net)$|[@.]example$'
while IFS= read -r -d '' path && IFS= read -r -d '' line && IFS= read -r text; do
  if [[ $path == Cargo.toml && $text =~ ^[[:space:]]*authors[[:space:]]*= ]]; then continue; fi
  rest=$text
  while [[ $rest =~ $email_re ]]; do
    addr=${BASH_REMATCH[0]}
    rest=${rest#*"$addr"}
    [[ $addr =~ $email_ok ]] || report "$path" "$line" private-email "$addr"
  done
done < <(grep_all -E "$email_re")

# 4. Wording. Every markdown file: no em dashes. The user docs (README, CONTRIBUTING,
#    docs/): no maturity hedges, no test provenance or measurements, and no history (issue
#    and PR numbers, "since 0.x", "used to"), which lives in CHANGELOG.md and in
#    docs/compatibility.md's feature table. An issue number is a `#N` outside code spans
#    and blocks; link targets such as `](#16-versioning)`, `owner/repo#N` and attributes
#    such as `#[non_exhaustive]` are not.
wording='
function out(rule, text) {
  sub(/^[ \t]+/, "", text)
  printf "%s:%d: %s: %s\n", name, FNR, rule, substr(text, 1, 120)
}
FNR == 1 {
  name = substr(FILENAME, 3)
  user = name ~ /^(README|CONTRIBUTING)\.md$/ || name ~ /^docs\/[^\/]+\.md$/
  history = user && name != "docs/compatibility.md"
  fence = ""
}
{
  line = $0
  low = " " tolower(line) " "
  if (index(line, "\342\200\224") || index(low, "&mdash;") || index(low, "&#8212;") || index(low, "&#x2014;"))
    out("em-dash", line)
  if (!user) next

  # Fenced code: the fence lines and everything between them.
  t = line
  sub(/^ */, "", t)
  f3 = substr(t, 1, 3)
  code = 0
  if (fence != "") {
    code = 1
    if (f3 == fence) fence = ""
  } else if (f3 == "```" || f3 == "~~~") {
    fence = f3
    code = 1
  }

  if (low ~ /[^a-z0-9_](experimental|pre-?alpha|untested|reports (are )?welcome|rough edges?)[^a-z0-9_]/)
    out("maturity", line)

  p = low
  gsub(/measured in [*_`]*(bytes|characters|chars|cells|columns|lines|utf-8|utf-16)/, "", p)
  if (p ~ /[^a-z0-9_](tested (on|against)|verified (under|against)|measured|medians?)[^a-z0-9_]/ ||
      p ~ /[^a-z0-9_]at commit [0-9a-f][0-9a-f][0-9a-f][0-9a-f][0-9a-f][0-9a-f][0-9a-f]/)
    out("provenance", line)

  if (!history) next
  hist = low ~ /[^a-z0-9_](since|as of|as in|before|added in) v?0\.[0-9]/
  s = low
  while (!hist && match(s, /[^a-z0-9_]used to[^a-z0-9_]/)) {
    # "is used to", "be used to" and the like mean "serves to", not history.
    w = substr(s, 1, RSTART)
    sub(/[^a-z]+$/, "", w)
    sub(/.*[^a-z]/, "", w)
    hist = w !~ /^(is|are|was|were|be|been|being|get|gets|got|getting|not|also|only|often)$/
    s = substr(s, RSTART + RLENGTH - 1)
  }
  if (hist) out("history", line)

  if (code) next
  s = line
  gsub(/`[^`]*`/, "", s)
  gsub(/\]\([^)]*\)/, "]", s)
  gsub(/(href|id|name)="[^"]*"/, "", s)
  if (s ~ /^ *\[[^]]*\]: /) s = ""
  if (s ~ /(^|[^A-Za-z0-9_&#])#[0-9]+([^A-Za-z0-9_-]|$)/) out("issue-ref", line)
}'
if ((${#markdown[@]})); then
  # The ./ prefix keeps awk from reading a name with `=` as an assignment.
  while IFS= read -r hit; do
    printf '%s\n' "$hit"
    violations=$((violations + 1))
  done < <(awk "$wording" "${markdown[@]/#/./}")
fi

# 5. Line budgets. Cut before adding.
for f in "${markdown[@]}"; do
  case $f in
  README.md) limit=220 ;;
  AGENTS.md) limit=150 ;;
  CONTRIBUTING.md) limit=150 ;;
  CHANGELOG.md) limit=450 ;;
  docs/architecture.md) limit=150 ;;
  docs/protocol.md) limit=1600 ;;
  docs/cli.md) limit=180 ;;
  docs/faq.md) limit=120 ;;
  docs/compatibility.md) limit=100 ;;
  docs/comparison.md) limit=120 ;;
  docs/*.md) limit=300 ;;
  bench/README.md) limit=120 ;;
  bench/RESULTS.md) limit=80 ;;
  *) continue ;;
  esac
  n=$(awk 'END { print NR }' "./$f")
  ((n <= limit)) || report "$f" $((limit + 1)) budget "$n lines, the limit is $limit"
done

if ((violations)); then
  echo "check-docs: $violations violation(s)"
  exit 1
fi
echo "check-docs: ok"
