// Shared, deterministic safety routing. No dependencies or code from PR heads.
const categories = ['database', 'ui', 'money', 'access', 'integration', 'platform-wide', 'release'];
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
    if (/^(crates\/|schemas\/|examples\/)/.test(path)) result.add('safety:integration');
    if (/^native\//.test(path)) result.add('safety:platform-wide');
    if (/^(\.github\/|scripts\/|skills\/)|(^|\/)(Cargo\.(toml|lock)|rust-toolchain(\.toml)?|AGENTS\.md|CLAUDE\.md|SKILL\.md)$/.test(path)) result.add('safety:release');
  }
  return [...result];
}

function route(labels, assignees = [], paths = [], inherited = []) {
  const next = new Set(labels);
  for (const label of [...pathRisks(paths), ...inherited]) {
    if (categories.some(c => label === `safety:${c}`) || label === 'needs-human-review') next.add(label);
  }
  const risk = categories.some(c => next.has(`safety:${c}`));
  const held = risk || next.has('needs-human-review');
  const complete = Object.entries(vocabulary).every(([prefix, values]) => {
    const found = [...next].filter(l => l.startsWith(`${prefix}:`));
    return found.length === 1 && values.includes(found[0].slice(prefix.length + 1));
  }) && assignees.some(a => a.type === 'User') &&
    [...next].filter(l => l.startsWith('safety:')).every(l => l === 'safety:none' || categories.some(c => l === `safety:${c}`)) &&
    (risk || next.has('safety:none'));
  if (risk) next.delete('safety:none');
  if (held) next.add('needs-human-review');
  const stateCount = states.filter(s => next.has(s)).length;
  let state;
  if (next.has('wontfix')) state = 'wontfix';
  else if (held) state = 'ready-for-human';
  else if (!complete) state = 'needs-info';
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
  // Re-fetch instead of trusting stale label-event snapshots.
  const { data: item } = await github.rest.issues.get({ ...repo, issue_number: number });
  if (item.state !== 'open') return;
  let paths = [];
  let inherited = [];
  if (event.pull_request) {
    const files = await github.paginate(github.rest.pulls.listFiles, { ...repo, pull_number: number, per_page: 100 });
    paths = files.flatMap(f => [f.filename, f.previous_filename].filter(Boolean));
    // Same-repository closing references only; full URLs are also supported.
    const escaped = `${repo.owner}/${repo.repo}`.replace(/[.*+?^${}()|[\]\\]/g, '\\$&');
    const refs = new RegExp(`\\b(?:close[sd]?|fix(?:e[sd])?|resolve[sd]?)\\s+(?:#|https://github\\.com/${escaped}/issues/)(\\d+)`, 'gi');
    for (const id of new Set([...String(item.body || '').matchAll(refs)].map(m => Number(m[1])))) {
      try {
        const { data: issue } = await github.rest.issues.get({ ...repo, issue_number: id });
        if (issue.pull_request) continue;
        inherited.push(...issue.labels.map(l => l.name));
        if (issue.state !== 'open' || !route(issue.labels.map(l => l.name), issue.assignees || []).includes('ready-for-agent')) {
          inherited.push('needs-human-review');
        }
      } catch (error) {
        // Unknown issue classification cannot make a PR eligible. Preserve
        // path routing even when a closing reference is missing/inaccessible.
        inherited.push('needs-human-review');
        core.warning(`Cannot classify linked issue #${id}; retaining a human-review hold.`);
        if (error.status !== 404) core.setFailed(`Linked issue #${id} could not be read.`);
      }
    }
  }
  const before = item.labels.map(l => l.name);
  const after = route(before, item.assignees, paths, inherited);
  const additions = after.filter(l => !before.includes(l));
  if (additions.length) await github.rest.issues.addLabels({ ...repo, issue_number: number, labels: additions });
  // Remove only policy-owned labels; never overwrite concurrent unrelated labels.
  for (const name of before.filter(l => !after.includes(l))) {
    try { await github.rest.issues.removeLabel({ ...repo, issue_number: number, name }); }
    catch (error) { if (error.status !== 404) throw error; }
  }
  core.info(`Safety routing checked #${number}; no review or merge performed.`);
  if (event.issue) {
    // A newly flagged issue must also flag its already-open linked PRs.
    const pulls = await github.paginate(github.rest.pulls.list, { ...repo, state: 'open', per_page: 100 });
    for (const pull of pulls) await sync({ github, core, context: { repo, payload: { pull_request: { number: pull.number } } } });
  }
}

module.exports = { pathRisks, route, sync };
