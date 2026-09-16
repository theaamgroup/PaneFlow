// Shared, deterministic safety routing. No dependencies or code from PR heads.
const categories = ['database', 'ui', 'money', 'access', 'integration', 'platform-wide', 'release'];
const maxClosingReferences = 20;
const maxLinkedPulls = 50;
const states = ['needs-info', 'ready-for-agent', 'ready-for-human', 'wontfix'];
const vocabulary = {
  severity: ['critical', 'high', 'medium', 'low'],
  area: ['app', 'terminal', 'agents', 'config', 'platform'],
  lens: ['correctness', 'security', 'perf', 'a11y', 'arch', 'data'],
};

function pathRisks(paths) {
  const result = new Set();
  for (const path of paths) {
    if (/^(src-app\/|assets\/|DESIGN\.md$)/.test(path)) result.add('safety:ui');
    if (/^(crates\/|schemas\/|examples\/|mcps\/)/.test(path)) result.add('safety:integration');
    if (/^native\//.test(path)) result.add('safety:platform-wide');
    if (/^(\.github\/|\.agents\/|\.claude\/|\.cursor\/|scripts\/|skills\/|packaging\/)|(^|\/)(Cargo\.(toml|lock)|rust-toolchain(\.toml)?|deny\.toml|clippy\.toml|AGENTS\.md|CLAUDE\.md|SKILL\.md)$/.test(path)) result.add('safety:release');
  }
  return [...result];
}

function hasHumanOwner(assignees) {
  return (assignees || []).some(a => a.type === 'User');
}

// Exactly one valid severity, area and lens label.
function classified(labels) {
  return Object.entries(vocabulary).every(([prefix, values]) => {
    const found = [...labels].filter(l => l.startsWith(`${prefix}:`));
    return found.length === 1 && values.includes(found[0].slice(prefix.length + 1));
  });
}

// A pull request carries no vocabulary labels of its own: its classification
// is inherited from its linked local issues. Returns the agreed metadata, or
// undefined when any linked issue is incomplete, they disagree, or no human
// owns any of them. Never adds vocabulary labels to the PR itself.
function deriveClassification(linked) {
  if (!Array.isArray(linked) || linked.length === 0) return undefined;
  const picks = {};
  for (const issue of linked) {
    const labels = [...(issue.labels || [])];
    if (route(labels, issue.assignees || []).includes('needs-info')) return undefined;
    for (const prefix of Object.keys(vocabulary)) {
      const value = labels.find(l => l.startsWith(`${prefix}:`));
      if (!(prefix in picks)) picks[prefix] = value;
      else if (picks[prefix] !== value) return undefined;
    }
  }
  if (!linked.some(issue => hasHumanOwner(issue.assignees))) return undefined;
  // Every complete issue without safety:none carries a risk category, which
  // the caller inherits onto the PR; so `every` and `some` agree here.
  picks.safetyNone = linked.every(issue => (issue.labels || []).includes('safety:none'));
  return picks;
}

// `linked` is undefined for an issue (classification read off the item) and an
// array of `{ labels, assignees }` for a pull request (classification derived
// from its linked issues; an empty array is a PR with nothing usable).
function route(labels, assignees = [], paths = [], inherited = [], requestedState, linked) {
  const next = new Set(labels);
  for (const label of [...pathRisks(paths), ...inherited]) {
    if (categories.some(c => label === `safety:${c}`) || label === 'needs-human-review') next.add(label);
  }
  const risk = categories.some(c => next.has(`safety:${c}`));
  const held = risk || next.has('needs-human-review');
  const isPull = linked !== undefined;
  const derived = isPull ? deriveClassification(linked) : undefined;
  const safetyValid = [...next].filter(l => l.startsWith('safety:')).every(l => l === 'safety:none' || categories.some(c => l === `safety:${c}`));
  const complete = safetyValid && (isPull
    ? Boolean(derived) && (risk || derived.safetyNone)
    : classified(next) && hasHumanOwner(assignees) && (risk || next.has('safety:none')));
  if (risk) next.delete('safety:none');
  if (held) next.add('needs-human-review');
  const stateCount = states.filter(s => next.has(s)).length;
  let state;
  if (!complete) state = 'needs-info';
  else if (next.has('wontfix')) state = 'wontfix';
  else if (held) state = 'ready-for-human';
  else if (['ready-for-agent', 'ready-for-human'].includes(requestedState) && next.has(requestedState)) state = requestedState;
  // A complete, unheld PR inherits eligibility from its ready-for-agent
  // issues: nobody promotes a PR by hand, so a stale needs-info must not stick.
  else if (isPull && !next.has('ready-for-agent') && !next.has('ready-for-human')) state = 'ready-for-agent';
  else if (stateCount !== 1) state = 'needs-info';
  if (state) {
    for (const name of states) next.delete(name);
    next.add(state);
  }
  return [...next].sort();
}

async function sync({ github, context, core }) {
  const repo = context.repo;
  const event = context.payload;
  const number = event.issue?.number || event.pull_request?.number;
  if (!number || event.issue?.pull_request) return;
  if (event.issue && ['deleted', 'transferred'].includes(event.action)) {
    // The original issue is no longer addressable in this repository.
    await syncOpenPulls({ github, core, repo });
    return;
  }
  // Re-fetch instead of trusting stale label-event snapshots.
  const { data: item } = await github.rest.issues.get({ ...repo, issue_number: number });
  if (item.state !== 'open') {
    if (event.issue) await syncOpenPulls({ github, core, repo, issueNumber: number });
    return;
  }
  let paths = [];
  let inherited = [];
  let linked;
  if (event.pull_request) {
    linked = [];
    const files = await github.paginate(github.rest.pulls.listFiles, { ...repo, pull_number: number, per_page: 100 });
    paths = files.flatMap(f => [f.filename, f.previous_filename].filter(Boolean));
    // GitHub resolves Markdown and manual links; examples in code/comments
    // cannot establish eligibility. Fetch one bounded page, never raw body IDs.
    let ids = [];
    let oversized = false;
    let linkedIssues = 0;
    try {
      const result = await github.graphql(`query($owner:String!,$repo:String!,$number:Int!,$limit:Int!) {
        repository(owner:$owner,name:$repo) {
          pullRequest(number:$number) {
            closingIssuesReferences(first:$limit) {
              pageInfo { hasNextPage }
              nodes { number repository { nameWithOwner } }
            }
          }
        }
      }`, { ...repo, number, limit: maxClosingReferences + 1 });
      const links = result.repository.pullRequest.closingIssuesReferences;
      oversized = links.pageInfo.hasNextPage || links.nodes.length > maxClosingReferences;
      const localRepo = `${repo.owner}/${repo.repo}`.toLowerCase();
      for (const issue of links.nodes) {
        if (issue.repository.nameWithOwner.toLowerCase() === localRepo) ids.push(issue.number);
        else inherited.push('needs-human-review');
      }
    } catch (error) {
      inherited.push('needs-human-review');
      core.setFailed('Closing issue references could not be read.');
    }
    // Oversized link sets get no per-issue requests: reserve API budget for
    // writing the hold and removing stale eligibility, even on repeated events.
    if (oversized) {
      inherited.push('needs-human-review');
      core.warning(`More than ${maxClosingReferences} closing references; retaining a human-review hold.`);
    }
    for (const id of oversized ? [] : ids) {
      try {
        const { data: issue } = await github.rest.issues.get({ ...repo, issue_number: id });
        if (issue.pull_request) continue;
        linkedIssues++;
        const labels = issue.labels.map(l => l.name);
        inherited.push(...labels);
        const moved = issue.repository_url && !issue.repository_url.toLowerCase().endsWith(`/repos/${repo.owner}/${repo.repo}`.toLowerCase());
        if (moved || issue.state !== 'open' || !route(labels, issue.assignees || []).includes('ready-for-agent')) {
          inherited.push('needs-human-review');
        }
        // Only open local issues classify the PR; closed or moved ones hold it.
        if (!moved && issue.state === 'open') linked.push({ labels, assignees: issue.assignees || [] });
      } catch (error) {
        // Unknown issue classification cannot make a PR eligible. Preserve
        // path routing even when a closing reference is missing/inaccessible.
        inherited.push('needs-human-review');
        core.warning(`Cannot classify linked issue #${id}; retaining a human-review hold.`);
        if (error.status !== 404) {
          core.setFailed(`Linked issue #${id} could not be read.`);
          break; // Do not compound throttling or service failures.
        }
      }
    }
    // Absent or unreadable issue links are unknown scope, never
    // evidence that this PR has an eligible issue for unattended work.
    if (linkedIssues === 0) inherited.push('needs-human-review');
  }
  const before = item.labels.map(l => l.name);
  const requestedState = event.action === 'labeled' ? event.label?.name : undefined;
  const after = route(before, item.assignees, paths, inherited, requestedState, linked);
  const additions = after.filter(l => !before.includes(l));
  if (additions.length) await github.rest.issues.addLabels({ ...repo, issue_number: number, labels: additions });
  // Remove only policy-owned labels; never overwrite concurrent unrelated labels.
  for (const name of before.filter(l => !after.includes(l))) {
    try { await github.rest.issues.removeLabel({ ...repo, issue_number: number, name }); }
    catch (error) { if (error.status !== 404) throw error; }
  }
  core.info(`Safety routing checked #${number}; no review or merge performed.`);
  // A newly flagged issue must also flag its already-open linked PRs. Body
  // and title edits change no label, owner, or link, so they trigger no sweep.
  if (event.issue && event.action !== 'edited') {
    await syncOpenPulls({ github, core, repo, issueNumber: number });
  }
}

// Open PRs linking one issue, from GitHub's resolved references (one bounded
// query). Returns undefined when the lookup fails so the caller can fall back.
async function linkedOpenPulls({ github, core, repo, issueNumber }) {
  try {
    const result = await github.graphql(`query($owner:String!,$repo:String!,$number:Int!,$limit:Int!) {
      repository(owner:$owner,name:$repo) {
        issue(number:$number) {
          closedByPullRequestsReferences(first:$limit, includeClosedPrs:false) {
            pageInfo { hasNextPage }
            nodes { number }
          }
        }
      }
    }`, { ...repo, number: issueNumber, limit: maxLinkedPulls });
    const links = result.repository.issue.closedByPullRequestsReferences;
    if (links.pageInfo.hasNextPage) {
      core.warning(`Issue #${issueNumber} has more than ${maxLinkedPulls} open linked pull requests; sweeping every open pull request.`);
      return undefined;
    }
    return links.nodes.map(n => n.number);
  } catch (error) {
    core.warning(`Linked pull requests of issue #${issueNumber} could not be read; sweeping every open pull request.`);
    return undefined;
  }
}

async function syncOpenPulls({ github, core, repo, issueNumber }) {
  let numbers;
  if (issueNumber !== undefined) numbers = await linkedOpenPulls({ github, core, repo, issueNumber });
  if (numbers === undefined) {
    const pulls = await github.paginate(github.rest.pulls.list, { ...repo, state: 'open', per_page: 100 });
    numbers = pulls.map(pull => pull.number);
  }
  for (const number of numbers) await sync({ github, core, context: { repo, payload: { pull_request: { number } } } });
}

module.exports = { pathRisks, route, deriveClassification, sync, syncOpenPulls };
