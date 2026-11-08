#!/bin/bash
# <xbar.title>mihoctl</xbar.title>
# <xbar.desc>mihomo core, selected node, subscription quota/expiry and last apply, from `mihoctl status --json`.</xbar.desc>
# <xbar.dependencies>mihoctl, jq</xbar.dependencies>
# <swiftbar.hideRunInTerminal>true</swiftbar.hideRunInTerminal>
#
# SwiftBar / xbar plugin. Install: symlink into the plugin folder; the `5m` in the name is the refresh interval.
# Warns (⚠) when the core is down, the last apply failed, a subscription failed to update,
# expires within $MIHOCTL_WARN_DAYS days (default 7) or used more than 90% of its quota.

export PATH="$HOME/.cargo/bin:/opt/homebrew/bin:/usr/local/bin:/usr/bin:/bin"
MIHOCTL="${MIHOCTL:-$(command -v mihoctl)}"
WARN_DAYS="${MIHOCTL_WARN_DAYS:-7}"
SELF="$(cd "$(dirname "$0")" && pwd)/$(basename "$0")"

if [ -z "$MIHOCTL" ]; then
  echo "mihoctl ✕"; echo "---"; echo "mihoctl not found in PATH | color=red"; exit 0
fi
# Menu action: build the zashboard URL (it carries the secret) only when clicked.
if [ "$1" = open-ui ]; then url="$("$MIHOCTL" ui)" && exec /usr/bin/open "$url"; exit 1; fi
err="$(mktemp)"; trap 'rm -f "$err"' EXIT
if ! json="$("$MIHOCTL" status --json 2>"$err")"; then
  echo "mihoctl ✕"; echo "---"; sed 's/|/¦/g; s/$/ | color=red/' "$err"; exit 0
fi

printf '%s' "$json" | jq -r --argjson now "$(date +%s)" --argjson warn_days "$WARN_DAYS" '
  def days: if . then ((. - $now) / 86400 | floor) else null end;
  def gib: . / 1073741824 * 10 | round / 10 | tostring + "G";
  def quota_warn: .userinfo as $u | $u != null and ($u.total // 0) > 1
    and (($u.upload // 0) + ($u.download // 0)) > $u.total * 0.9;
  def expiry_warn: (.userinfo.expire // 0) > 0 and (.userinfo.expire | days) < $warn_days;
  def sub_warn: .last_error != null or quota_warn or expiry_warn;
  def clean: tostring | gsub("\\|"; "¦") | gsub("\n"; " ");
  def show: if . == null then "?" else clean end;

  (.core.state == "running") as $up
  | ($up and (.last_apply.ok != false) and ([.subscriptions[] | sub_warn] | any | not)) as $ok
  | (if $up then (.core.group_now // "?") else "core down" end) as $title
  | "\(if $ok then "◉" else "⚠" end) \($title | clean) | length=30",
    "---",
    (if $up then "mihomo \(.core.version | show) · \(.core.mode | show) · tun=\(.core.tun | show) · :\(.core.mixed_port | show)",
       (.core.error | select(.) | "\(clean) | color=red")
     else "core \(.core.state) at \(.core.controller)\(if .core.error then ": " + (.core.error | clean) else "" end) | color=red"
     end),
    "\(.core.group | clean) → \(.core.group_now | show)",
    "launchd core=\(.launchd.core) refresh=\(.launchd.refresh)\(if (.launchd.core and .launchd.refresh) then "" else " | color=orange" end)",
    "---",
    (.subscriptions[]
     | ([ "\(.nodes) nodes",
          (.userinfo | select(. != null and (.total // 0) > 1)
           | "\((.upload // 0) + (.download // 0) | gib)/\(.total | gib)"),
          (.userinfo.expire | select(. != null and . > 0) | "\(days)d left") ] | join(" · ")) as $info
     | "\(.name): \($info)\(if sub_warn then " | color=orange" else "" end)",
       (select(.last_error) | "--failed: \(.last_error | clean) | color=red")),
    "---",
    (.last_apply | select(. != null)
     | "last apply \(if .ok then "ok" else "FAILED" end): \(.message | clean)\(if .ok then "" else " | color=red" end)")
'

echo "---"
echo "Refresh now | bash=\"$MIHOCTL\" param1=refresh terminal=false refresh=true"
echo "Show pending diff | bash=\"$MIHOCTL\" param1=diff terminal=true"
echo "Open zashboard | bash=\"$SELF\" param1=open-ui terminal=false"
echo "Open refresh log | bash=/usr/bin/open param1=\"$HOME/Library/Logs/mihoctl/refresh.log\" terminal=false"
# The core log is root-only (it may quote whatever the config points at): tail it via sudo in a terminal.
echo "Tail core log (sudo) | bash=/usr/bin/sudo param1=/usr/bin/tail param2=-f param3=/Library/Logs/mihoctl/core.log terminal=true"
