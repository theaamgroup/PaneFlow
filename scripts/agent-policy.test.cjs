const { test } = require('node:test');
const assert = require('node:assert/strict');
const { readFileSync } = require('node:fs');
const path = require('node:path');
const { route, pathRisks, pathHolds, holdCategories, deriveClassification, sync } = require('./agent-policy.cjs');
const owner = [{ type: 'User', login: 'maintainer' }];
const base = ['severity:high', 'area:app', 'lens:correctness', 'safety:none', 'ready-for-agent'];
const repo = { owner: 'org', repo: 'repo' };
const quiet = { info() {}, warning() {} };

// A pull request as GitHub actually delivers it: no vocabulary labels and no
// assignee. `labels` defaults to what a previous routing run left behind.
function pullFixture(labels = ['needs-info'], extra = {}) {
  return { state: 'open', labels: labels.map(name => ({ name })), assignees: [], ...extra };
}
function issueFixture(labels = base, assignees = owner, extra = {}) {
  return { repository_url: 'https://api.github.com/repos/org/repo', state: 'open', labels: labels.map(name => ({ name })), assignees, ...extra };
}

// GraphQL stub answering both resolved-reference queries: the PR's closing
// issues (`ids`) and an issue's open linked PRs (`pulls`).
function linkedReferences(ids = [9], hasNextPage = false, pulls = [7], pullsHasNextPage = false) {
  return async (query, variables) => {
    assert.equal(variables.owner, 'org');
    assert.equal(variables.repo, 'repo');
    if (query.includes('closedByPullRequestsReferences')) {
      assert.equal(variables.limit, 50);
      assert.match(query, /includeClosedPrs:\s*false/);
      return { repository: { issue: { closedByPullRequestsReferences: {
        pageInfo: { hasNextPage: pullsHasNextPage },
        nodes: pulls.map(number => ({ number })),
      } } } };
    }
    assert.equal(variables.limit, 21);
    return { repository: { pullRequest: { closingIssuesReferences: {
      pageInfo: { hasNextPage },
      nodes: ids.map(number => ({ number, repository: { nameWithOwner: 'org/repo' } })),
    } } } };
  };
}

// Routes PR #7 against `issues` (number -> issue fixture) and returns the
// label set the PR ends with plus the raw writes.
async function routePull({ issues = { 9: issueFixture() }, pull = pullFixture(), files = [{ filename: 'docs/guide.md' }], graphql = linkedReferences(Object.keys(issues).map(Number)), core = quiet } = {}) {
  const labels = new Set(pull.labels.map(l => l.name));
  const added = [], removed = [], reads = [];
  const github = {
    graphql,
    paginate: async () => files,
    rest: {
      pulls: { listFiles: 'files' },
      issues: {
        get: async ({ issue_number }) => {
          reads.push(issue_number);
          if (issue_number === 7) return { data: pull };
          if (issues[issue_number] instanceof Error) throw issues[issue_number];
          if (!issues[issue_number]) throw Object.assign(new Error('missing'), { status: 404 });
          return { data: issues[issue_number] };
        },
        addLabels: async ({ labels: names }) => { added.push(...names); names.forEach(l => labels.add(l)); },
        removeLabel: async ({ name }) => { removed.push(name); labels.delete(name); },
      },
    },
  };
  await sync({ github, context: { repo, payload: { pull_request: { number: 7 } } }, core });
  return { labels: [...labels].sort(), added, removed, reads };
}

test('privileged policy checkout uses protected main, including stacked PRs', () => {
  const workflow = readFileSync(path.join(__dirname, '../.github/workflows/agent-safety.yml'), 'utf8');
  const refs = workflow.split('\n').filter(line => /^\s+ref:/.test(line));
  assert.deepEqual(refs, ['          ref: refs/heads/main']);
  assert.ok(!workflow.includes('pull_request.base.sha'));
  assert.ok(!workflow.includes('pull_request.head.sha'));
  assert.ok(workflow.includes('persist-credentials: false'));
});

test('complete safe issue remains eligible; unrelated labels survive', () => {
  assert.deepEqual(route([...base, 'bug'], owner), [...base, 'bug'].sort());
});
assert.deepEqual(holdCategories, ['database', 'money', 'access', 'platform-wide']);
for (const category of holdCategories) {
  test(`${category} always holds unattended work`, () => {
    const result = route([...base, `safety:${category}`], owner);
    assert.ok(result.includes('needs-human-review'));
    assert.ok(result.includes('ready-for-human'));
    assert.ok(!result.includes('ready-for-agent'));
    assert.ok(!result.includes('safety:none'));
  });
}
// The routine categories are recorded but never demand a human on their own:
// nearly every change touches src-app/ or crates/, so they would hold all work.
for (const category of ['ui', 'integration', 'release']) {
  test(`${category} is recorded without a human hold`, () => {
    const result = route([...base, `safety:${category}`], owner);
    assert.ok(!result.includes('needs-human-review'));
    assert.ok(result.includes('ready-for-agent'));
    assert.ok(result.includes(`safety:${category}`));
    assert.ok(!result.includes('safety:none'));
  });
}
test('severity:critical always holds unattended work', () => {
  const result = route(base.map(l => l === 'severity:high' ? 'severity:critical' : l), owner);
  assert.ok(result.includes('needs-human-review'));
  assert.ok(result.includes('ready-for-human'));
  assert.ok(!result.includes('ready-for-agent'));
});
test('missing or bot-only owner blocks unattended work', () => {
  for (const owners of [[], [{ type: 'Bot' }]]) assert.ok(route(base, owners).includes('needs-info'));
});

test('incomplete risky items retain needs-info and their human hold', () => {
  for (const labels of [base.filter(l => l !== 'severity:high'), base]) {
    const result = route([...labels, 'safety:access'], []);
    assert.ok(result.includes('needs-info'));
    assert.ok(result.includes('needs-human-review'));
    assert.ok(!result.includes('ready-for-agent'));
    assert.ok(!result.includes('ready-for-human'));
  }
});
test('unknown, missing, and conflicting metadata fail closed', () => {
  for (const labels of [base.filter(l => l !== 'safety:none'), base.filter(l => l !== 'severity:high'), [...base, 'safety:unknown'], [...base, 'severity:low'], [...base, 'ready-for-human']]) {
    const result = route(labels, owner);
    assert.ok(result.includes('needs-info'));
    assert.ok(!result.includes('ready-for-agent'));
  }
});
test('existing human hold is never automatically cleared', () => {
  assert.ok(route([...base, 'needs-human-review'], owner).includes('needs-human-review'));
  assert.ok(!route([...base, 'needs-human-review'], owner).includes('ready-for-agent'));
});
test('paths and linked issue flags add risk; renames handled by caller', () => {
  assert.deepEqual(pathRisks(['packaging/macos/paneflow.entitlements']), ['safety:release']);
  assert.deepEqual(pathRisks(['mcps/paneflow/tools/read_pane.json']), ['safety:integration']);
  for (const p of ['deny.toml', 'clippy.toml', '.cursor/rules/review.mdc', '.claude/settings.json']) assert.deepEqual(pathRisks([p]), ['safety:release']);
  assert.deepEqual(pathRisks(['skills/paneflow-conductor/SKILL.md']), ['safety:release']);
  assert.deepEqual(pathRisks(['.agents/skills/example/SKILL.md']), ['safety:release']);
  assert.deepEqual(pathRisks(['src-app/src/main.rs', '.github/workflows/test.yml', 'crates/x/src/lib.rs', 'native/a']), ['safety:ui', 'safety:release', 'safety:integration', 'safety:platform-wide']);
  assert.ok(route(base, owner, [], ['safety:access']).includes('needs-human-review'));
  assert.deepEqual(pathRisks(['docs/guide.md']), []);
});
test('only release-pipeline, signing, and policy paths hold; routine paths only label', () => {
  const critical = ['.github/workflows/release.yml', '.github/workflows/agent-safety.yml', 'scripts/agent-policy.cjs', 'scripts/agent-policy.test.cjs', 'scripts/sparkle-dist.sh', 'scripts/bundle-macos.sh', 'scripts/create-dmg.sh', 'packaging/macos/paneflow.entitlements'];
  for (const p of critical) assert.ok(pathHolds([p]), p);
  const routine = ['src-app/src/main.rs', 'crates/x/src/lib.rs', '.github/workflows/run_tests.yml', 'scripts/bench-terminal.sh', 'Cargo.lock', 'CLAUDE.md', 'AGENTS.md', '.claude/settings.json', 'docs/guide.md'];
  for (const p of routine) assert.ok(!pathHolds([p]), p);
  assert.ok(pathHolds([...routine, 'scripts/create-dmg.sh']));
  assert.ok(route(base, owner, ['.github/workflows/release.yml']).includes('needs-human-review'));
  const ui = route(base, owner, ['src-app/src/main.rs']);
  assert.ok(!ui.includes('needs-human-review'));
  assert.ok(ui.includes('safety:ui'));
  assert.ok(ui.includes('ready-for-agent'));
});

// --- Pull requests inherit classification from their linked issues ---------

test('deriveClassification agrees on one value per prefix and needs a human owner', () => {
  const issue = { labels: base, assignees: owner };
  assert.deepEqual(deriveClassification([issue]), { severity: 'severity:high', area: 'area:app', lens: 'lens:correctness', safetyNone: true });
  assert.deepEqual(deriveClassification([issue, issue]), deriveClassification([issue]));
  assert.equal(deriveClassification([]), undefined);
  assert.equal(deriveClassification([{ labels: base, assignees: [] }]), undefined);
  assert.equal(deriveClassification([{ labels: base, assignees: [{ type: 'Bot' }] }]), undefined);
  assert.equal(deriveClassification([issue, { labels: base.map(l => l === 'severity:high' ? 'severity:low' : l), assignees: owner }]), undefined);
  assert.equal(deriveClassification([issue, { labels: base.filter(l => l !== 'lens:correctness'), assignees: owner }]), undefined);
  const risky = { labels: [...base.filter(l => l !== 'safety:none'), 'safety:ui'], assignees: owner };
  assert.equal(deriveClassification([risky]).safetyNone, false);
  assert.equal(deriveClassification([issue, risky]).safetyNone, false);
});

test('a PR never receives vocabulary labels of its own', async () => {
  const { labels, added } = await routePull();
  assert.ok(!added.some(l => /^(severity|area|lens):/.test(l)));
  assert.ok(!added.includes('safety:none'));
  assert.deepEqual(labels, ['ready-for-agent']);
});

test('realistic PR: one classified safety:none issue with a human owner and no path risk is ready-for-agent', async () => {
  const { labels, added, removed } = await routePull({ pull: pullFixture(['needs-info']) });
  assert.deepEqual(labels, ['ready-for-agent']);
  assert.ok(!added.includes('needs-human-review'));
  assert.ok(removed.includes('needs-info'));
});

test('realistic PR: the same issue touching src-app is labelled safety:ui and stays ready-for-agent', async () => {
  const { labels } = await routePull({ pull: pullFixture(['needs-info']), files: [{ filename: 'src-app/src/main.rs' }] });
  assert.deepEqual(labels, ['ready-for-agent', 'safety:ui']);
});

test('realistic PR: a linked severity:critical issue holds the PR', async () => {
  const { labels } = await routePull({ issues: { 9: issueFixture(base.map(l => l === 'severity:high' ? 'severity:critical' : l)) } });
  assert.deepEqual(labels, ['needs-human-review', 'ready-for-human']);
});

test('realistic PR: a critical path holds even with a safe linked issue', async () => {
  const { labels } = await routePull({ files: [{ filename: '.github/workflows/release.yml' }] });
  assert.deepEqual(labels, ['needs-human-review', 'ready-for-human', 'safety:release']);
});

test('realistic PR: docs-only with no linked issue is needs-info without a hold', async () => {
  const { labels, added } = await routePull({ pull: pullFixture([]), graphql: linkedReferences([]) });
  assert.deepEqual(labels, ['needs-info']);
  assert.deepEqual(added, ['needs-info']);
});

test('realistic PR: two linked issues that agree still classify the PR', async () => {
  const { labels } = await routePull({ issues: { 9: issueFixture(), 10: issueFixture() } });
  assert.deepEqual(labels, ['ready-for-agent']);
});

test('realistic PR: two linked issues disagreeing on severity keep needs-info', async () => {
  const { labels } = await routePull({ issues: { 9: issueFixture(), 10: issueFixture(base.map(l => l === 'severity:high' ? 'severity:low' : l)) } });
  assert.ok(labels.includes('needs-info'));
  assert.ok(!labels.includes('ready-for-agent'));
  assert.ok(!labels.includes('ready-for-human'));
});

test('realistic PR: a linked issue lacking a human assignee keeps needs-info', async () => {
  for (const assignees of [[], [{ type: 'Bot', login: 'bot' }]]) {
    const { labels } = await routePull({ issues: { 9: issueFixture(base, assignees) } });
    assert.ok(labels.includes('needs-info'), JSON.stringify(assignees));
    assert.ok(!labels.includes('needs-human-review'));
    assert.ok(!labels.includes('ready-for-agent'));
  }
});

test('realistic PR: a stale ready-for-agent is withdrawn when the linked issue loses its metadata', async () => {
  const { labels } = await routePull({ pull: pullFixture(['ready-for-agent']), issues: { 9: issueFixture(base.filter(l => l !== 'area:app')) } });
  assert.deepEqual(labels, ['needs-info']);
});

test('realistic PR: a human hold on the PR itself survives a fully classified issue', async () => {
  const { labels } = await routePull({ pull: pullFixture(['needs-human-review']) });
  assert.deepEqual(labels, ['needs-human-review', 'ready-for-human']);
});

for (const variant of ['unlinked', 'foreign-redirect', 'pr-link', 'missing-safety', 'missing-owner', 'needs-info', 'closed']) {
  test(`linked issue eligibility fails closed: ${variant}`, async () => {
    const issue = variant === 'missing-safety' ? issueFixture(base.filter(l => l !== 'safety:none'))
      : variant === 'needs-info' ? issueFixture(base.map(l => l === 'ready-for-agent' ? 'needs-info' : l))
      : variant === 'missing-owner' ? issueFixture(base, [])
      : variant === 'closed' ? issueFixture(base, owner, { state: 'closed' })
      : variant === 'pr-link' ? issueFixture(base, owner, { pull_request: {} })
      : variant === 'foreign-redirect' ? issueFixture(base, owner, { repository_url: 'https://api.github.com/repos/other/project' })
      : issueFixture();
    const { labels } = await routePull({ issues: { 9: issue }, graphql: linkedReferences(variant === 'unlinked' ? [] : [9]) });
    // Ineligible for unattended work, but not a critical change: no human hold.
    assert.ok(!labels.includes('needs-human-review'), variant);
    assert.ok(labels.includes('needs-info'), variant);
    assert.ok(!labels.includes('ready-for-agent'), variant);
  });
}

test('closing keywords in the PR body never establish a link', async () => {
  // Static guard: the policy must not read `body` at all.
  const source = readFileSync(path.join(__dirname, 'agent-policy.cjs'), 'utf8');
  assert.ok(!/\bbody\b/.test(source.replace(/\/\/.*$/gm, '')), 'agent-policy.cjs reads item.body');
  // Behavioural guard: a body full of closing keywords with no resolved reference is unlinked.
  const body = 'Closes #9\nFixes org/repo#9\nCloses: #9\n<!-- Closes #9 -->\n`Closes #9`\n```\nCloses #9\n```';
  const { labels, reads } = await routePull({ pull: pullFixture(['ready-for-agent'], { body }), graphql: linkedReferences([]), files: [{ filename: 'src-app/main.rs' }] });
  assert.deepEqual(reads, [7]);
  assert.deepEqual(labels, ['needs-info', 'safety:ui']);
});

test('routing is idempotent and never promotes needs-info', () => {
  const first = route([...base, 'safety:ui'], owner);
  assert.deepEqual(route(first, owner), first);
  assert.ok(route(base.map(l => l === 'ready-for-agent' ? 'needs-info' : l), owner).includes('needs-info'));
});
test('PR routing is idempotent', async () => {
  const first = await routePull({ files: [{ filename: 'src-app/main.rs' }] });
  const second = await routePull({ pull: pullFixture(first.labels), files: [{ filename: 'src-app/main.rs' }] });
  assert.deepEqual(second.labels, first.labels);
  assert.deepEqual(second.added, []);
  assert.deepEqual(second.removed, []);
});
test('closed/wontfix issues are not reopened for agent work', () => {
  assert.ok(!route([...base, 'wontfix'], owner).includes('ready-for-agent'));
});
test('sync reads current state, inherits risk and writes only label deltas', async () => {
  const { added, labels } = await routePull({
    pull: pullFixture(['needs-info']),
    files: [{ filename: 'docs/new.md', previous_filename: 'src-app/old.rs' }],
    issues: { 9: issueFixture([...base.filter(l => l !== 'safety:none'), 'safety:access']) },
  });
  assert.ok(added.includes('safety:ui'));
  assert.ok(added.includes('safety:access'));
  assert.ok(added.includes('needs-human-review'));
  assert.ok(!added.includes('needs-info'));
  assert.deepEqual(labels, ['needs-human-review', 'ready-for-human', 'safety:access', 'safety:ui']);
});

// --- Issue events sweep only the PRs that link the issue -------------------

function issueEventGithub({ issueState = 'open', issueLabels = [...base, 'safety:access'], graphql = linkedReferences(), pulls = [{ number: 7 }, { number: 8 }] } = {}) {
  const writes = [], reads = [], paginated = [], warnings = [];
  const github = {
    graphql,
    paginate: async (method) => { paginated.push(method); return method === 'pulls' ? pulls : []; },
    rest: {
      pulls: { list: 'pulls', listFiles: 'files' },
      issues: {
        get: async ({ owner: repoOwner, repo: name, issue_number }) => {
          assert.equal(repoOwner, 'org');
          assert.equal(name, 'repo');
          reads.push(issue_number);
          if (issue_number === 9) return { data: issueFixture(issueLabels, owner, { state: issueState }) };
          return { data: pullFixture(['ready-for-agent']) };
        },
        addLabels: async (args) => writes.push(args),
        removeLabel: async () => {},
      },
    },
  };
  return { github, writes, reads, paginated, core: { info() {}, warning: m => warnings.push(m) }, warnings };
}

for (const issueState of ['open', 'closed']) {
  test(`issue ${issueState} event propagates to an existing linked PR`, async () => {
    const { github, writes, core } = issueEventGithub({ issueState });
    class Context {
      get repo() { return repo; }
    }
    const context = new Context();
    context.payload = { action: 'labeled', label: { name: 'safety:access' }, issue: { number: 9 } };
    await sync({ github, context, core });
    assert.ok(writes.some(w => w.issue_number === 7 && w.labels.includes('needs-human-review')));
    if (issueState === 'closed') assert.ok(writes.every(w => w.issue_number !== 9));
  });
}

test('issue event sweeps only the open PRs that link the issue, never the whole list', async () => {
  const { github, writes, reads, paginated, core } = issueEventGithub();
  await sync({ github, context: { repo, payload: { action: 'labeled', label: { name: 'safety:access' }, issue: { number: 9 } } }, core });
  assert.ok(!paginated.includes('pulls'), 'pulls.list was paginated');
  assert.deepEqual(reads, [9, 7, 9]);
  assert.ok(writes.some(w => w.issue_number === 7 && w.labels.includes('needs-human-review')));
  assert.ok(writes.every(w => w.issue_number !== 8));
});

test('issue edited event routes the issue but skips the PR sweep', async () => {
  const graphqlCalls = [];
  const { github, writes, reads, paginated, core } = issueEventGithub({ graphql: async (query) => { graphqlCalls.push(query); throw new Error('must not be called'); } });
  await sync({ github, context: { repo, payload: { action: 'edited', issue: { number: 9 } } }, core });
  assert.deepEqual(reads, [9]);
  assert.deepEqual(graphqlCalls, []);
  assert.deepEqual(paginated, []);
  assert.ok(writes.every(w => w.issue_number === 9));
});

test('issue sweep falls back to every open PR when the linked-PR query fails', async () => {
  const { github, writes, reads, paginated, core, warnings } = issueEventGithub({
    graphql: async (query, variables) => {
      if (query.includes('closedByPullRequestsReferences')) throw new Error('API unavailable');
      return linkedReferences()(query, variables);
    },
  });
  await sync({ github, context: { repo, payload: { action: 'labeled', label: { name: 'safety:access' }, issue: { number: 9 } } }, core });
  assert.deepEqual(paginated.filter(m => m === 'pulls'), ['pulls']);
  assert.deepEqual(reads, [9, 7, 9, 8, 9]);
  assert.ok(writes.some(w => w.issue_number === 7 && w.labels.includes('needs-human-review')));
  assert.ok(writes.some(w => w.issue_number === 8 && w.labels.includes('needs-human-review')));
  assert.equal(warnings.length, 1);
});

test('issue sweep falls back to every open PR when more than 50 PRs link the issue', async () => {
  const { github, paginated, core } = issueEventGithub({ graphql: linkedReferences([9], false, [7], true) });
  await sync({ github, context: { repo, payload: { action: 'labeled', label: { name: 'safety:access' }, issue: { number: 9 } } }, core });
  assert.deepEqual(paginated.filter(m => m === 'pulls'), ['pulls']);
});

test('issue closure is subscribed in the actual workflow', () => {
  const workflow = readFileSync(path.join(__dirname, '../.github/workflows/agent-safety.yml'), 'utf8');
  assert.match(workflow, /issues:\s*\n\s*types: \[[^\]]*\bclosed\b/);
  assert.match(workflow, /issues:\s*\n\s*types: \[[^\]]*\bdeleted\b/);
  assert.match(workflow, /issues:\s*\n\s*types: \[[^\]]*\btransferred\b/);
});

for (const action of ['deleted', 'transferred']) {
  test(`issue ${action} event sweeps every open PR without fetching the event issue`, async () => {
    const calls = [], additions = [], paginated = [];
    const github = {
      graphql: linkedReferences(),
      paginate: async method => { paginated.push(method); return method === 'pulls' ? [{ number: 7 }] : []; },
      rest: {
        pulls: { list: 'pulls', listFiles: 'files' },
        issues: {
          get: async ({ issue_number }) => {
            calls.push(issue_number);
            if (issue_number === 9) throw Object.assign(new Error('missing'), { status: 404 });
            return { data: pullFixture(['ready-for-agent']) };
          },
          addLabels: async ({ labels }) => additions.push(...labels),
          removeLabel: async () => {},
        },
      },
    };
    await sync({ github, context: { repo, payload: { action, issue: { number: 9 } } }, core: quiet });
    assert.deepEqual(paginated.filter(m => m === 'pulls'), ['pulls']);
    assert.deepEqual(calls, [7, 9]);
    assert.ok(additions.includes('needs-info'));
    assert.ok(!additions.includes('needs-human-review'));
  });
}

for (const status of [404, 403, 500]) {
  test(`unreadable linked issue (${status}) retains path labels and withdraws eligibility`, async () => {
    const failures = [];
    const { added, labels } = await routePull({
      issues: { 999: Object.assign(new Error('unavailable'), { status }) },
      files: [{ filename: 'src-app/main.rs' }],
      core: { info() {}, warning() {}, setFailed(message) { failures.push(message); } },
    });
    assert.ok(added.includes('safety:ui'));
    assert.ok(!added.includes('needs-human-review'));
    assert.ok(labels.includes('needs-info'));
    assert.equal(failures.length, status === 404 ? 0 : 1);
  });
}

test('all four fork-runner guards from b227ca1 remain intact', () => {
  const workflow = readFileSync(path.join(__dirname, '../.github/workflows/run_tests.yml'), 'utf8');
  const guards = workflow.split('\n').filter(line => line.includes('runs-on:') && line.includes('head.repo.full_name'));
  // The self-hosted arm names the runner's full label set so a future
  // self-hosted runner on another OS cannot pick these jobs up.
  const expected = "    runs-on: ${{ (github.event_name != 'pull_request' || github.event.pull_request.head.repo.full_name == github.repository) && fromJSON('[\"self-hosted\",\"Linux\",\"X64\"]') || 'ubuntu-24.04' }}";
  assert.deepEqual(guards, Array(4).fill(expected));
});

for (const state of ['ready-for-agent', 'ready-for-human']) {
  for (const variant of ['complete', 'missing-owner', 'held', 'stale']) {
    test(`explicit ${state} promotion: ${variant}`, async () => {
      const labels = new Set(base.filter(l => l !== 'ready-for-agent'));
      labels.add('needs-info');
      if (variant !== 'stale') labels.add(state);
      if (variant === 'held') labels.add('needs-human-review');
      const github = {
        graphql: linkedReferences([9], false, []),
        paginate: async () => [],
        rest: {
          pulls: { list: 'pulls' },
          issues: {
            get: async () => ({ data: { state: 'open', labels: [...labels].map(name => ({ name })), assignees: variant === 'missing-owner' ? [] : owner } }),
            addLabels: async ({ labels: added }) => added.forEach(l => labels.add(l)),
            removeLabel: async ({ name }) => labels.delete(name),
          },
        },
      };
      const context = { repo, payload: { action: 'labeled', label: { name: state }, issue: { number: 9 } } };
      await sync({ github, context, core: quiet });
      const expected = variant === 'held' ? 'ready-for-human' : ['missing-owner', 'stale'].includes(variant) ? 'needs-info' : state;
      assert.deepEqual([...labels].filter(l => ['needs-info', 'ready-for-agent', 'ready-for-human', 'wontfix'].includes(l)), [expected]);
      assert.equal(labels.has('needs-human-review'), variant === 'held');
    });
  }
}

test('open wontfix requires complete metadata and a human owner', () => {
  for (const missing of ['severity:high', 'area:app', 'lens:correctness', 'safety:none', 'owner']) {
    const result = route([...base.filter(l => l !== missing), 'wontfix', 'needs-info'], missing === 'owner' ? [] : owner);
    assert.ok(result.includes('needs-info'), missing);
    assert.ok(!result.includes('wontfix'), missing);
    assert.ok(!result.includes('ready-for-agent'), missing);
  }
  const complete = route([...base, 'wontfix', 'needs-info'], owner);
  assert.ok(complete.includes('wontfix'));
  assert.ok(!complete.includes('needs-info'));
  const held = route(['wontfix', 'safety:access'], []);
  assert.ok(held.includes('needs-info'));
  assert.ok(held.includes('needs-human-review'));
});

for (const count of [20, 21, 3000]) {
  test(`${count} closing references have bounded requests and withdraw eligibility`, async () => {
    const ids = Array.from({ length: Math.min(count, 21) }, (_, i) => i + 100);
    const { reads, added, labels } = await routePull({
      pull: pullFixture(['ready-for-agent']),
      issues: {},
      files: [{ filename: 'src-app/main.rs' }],
      graphql: linkedReferences(ids, count > 21),
    });
    assert.equal(reads.length, count > 20 ? 1 : 21);
    assert.ok(added.includes('needs-info'));
    assert.ok(!added.includes('needs-human-review'));
    assert.ok(added.includes('safety:ui'));
    assert.ok(!labels.includes('ready-for-agent'));
  });
}

test('throttled lookups stop at the first failure and still withdraw eligibility', async () => {
  const failures = [];
  const throttled = () => Object.assign(new Error('throttled'), { status: 429 });
  const { reads, added, labels } = await routePull({
    pull: pullFixture(['ready-for-agent']),
    issues: { 9: throttled(), 10: throttled() },
    files: [],
    core: { info() {}, warning() {}, setFailed(message) { failures.push(message); } },
  });
  assert.deepEqual(reads, [7, 9]);
  assert.equal(failures.length, 1);
  assert.ok(added.includes('needs-info'));
  assert.ok(!added.includes('needs-human-review'));
  assert.ok(!labels.includes('ready-for-agent'));
});

for (const variant of ['query-failure', 'foreign-issue']) {
  test(`resolved GitHub references fail closed: ${variant}`, async () => {
    const failures = [];
    const { added, labels, reads } = await routePull({
      pull: pullFixture(['ready-for-agent']),
      files: [{ filename: 'src-app/main.rs' }],
      graphql: async () => {
        if (variant === 'query-failure') throw new Error('API unavailable');
        return { repository: { pullRequest: { closingIssuesReferences: {
          pageInfo: { hasNextPage: false },
          nodes: [{ number: 9, repository: { nameWithOwner: 'other/repo' } }],
        } } } };
      },
      core: { info() {}, setFailed(message) { failures.push(message); } },
    });
    assert.deepEqual(reads, [7]);
    assert.ok(added.includes('needs-info'));
    assert.ok(!added.includes('needs-human-review'));
    assert.ok(added.includes('safety:ui'));
    assert.ok(!labels.includes('ready-for-agent'));
    assert.equal(failures.length, variant === 'query-failure' ? 1 : 0);
  });
}
