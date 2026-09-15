const { test } = require('node:test');
const assert = require('node:assert/strict');
const { route, pathRisks, sync } = require('./agent-policy.cjs');
const owner = [{ type: 'User', login: 'maintainer' }];
const base = ['severity:high', 'area:app', 'lens:correctness', 'safety:none', 'ready-for-agent'];

test('privileged policy checkout uses protected main, including stacked PRs', () => {
  const { readFileSync } = require('node:fs');
  const workflow = readFileSync(require('node:path').join(__dirname, '../.github/workflows/agent-safety.yml'), 'utf8');
  const refs = workflow.split('\n').filter(line => /^\s+ref:/.test(line));
  assert.deepEqual(refs, ['          ref: refs/heads/main']);
  assert.ok(!workflow.includes('pull_request.base.sha'));
  assert.ok(!workflow.includes('pull_request.head.sha'));
  assert.ok(workflow.includes('persist-credentials: false'));
});

test('complete safe issue remains eligible; unrelated labels survive', () => {
  assert.deepEqual(route([...base, 'bug'], owner), [...base, 'bug'].sort());
});
for (const category of ['database', 'ui', 'money', 'access', 'integration', 'platform-wide', 'release']) {
  test(`${category} always holds unattended work`, () => {
    const result = route([...base, `safety:${category}`], owner);
    assert.ok(result.includes('needs-human-review'));
    assert.ok(result.includes('ready-for-human'));
    assert.ok(!result.includes('ready-for-agent'));
    assert.ok(!result.includes('safety:none'));
  });
}
test('missing or bot-only owner blocks unattended work', () => {
  for (const owners of [[], [{ type: 'Bot' }]]) assert.ok(route(base, owners).includes('needs-info'));
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
  assert.deepEqual(pathRisks(['skills/paneflow-conductor/SKILL.md']), ['safety:release']);
  assert.deepEqual(pathRisks(['.agents/skills/example/SKILL.md']), ['safety:release']);
  assert.deepEqual(pathRisks(['src-app/src/main.rs', '.github/workflows/test.yml', 'crates/x/src/lib.rs', 'native/a']), ['safety:ui', 'safety:release', 'safety:integration', 'safety:platform-wide']);
  assert.ok(route(base, owner, [], ['safety:access']).includes('needs-human-review'));
  assert.deepEqual(pathRisks(['docs/guide.md']), []);
});

for (const variant of ['eligible', 'qualified', 'colon', 'missing-link', 'foreign-link', 'pr-link', 'missing-safety', 'missing-owner', 'needs-info', 'closed']) {
  test(`linked issue eligibility: ${variant}`, async () => {
    const additions = [], removals = [];
    const issueLabels = variant === 'missing-safety' ? base.filter(l => l !== 'safety:none')
      : variant === 'needs-info' ? base.map(l => l === 'ready-for-agent' ? 'needs-info' : l) : base;
    const github = {
      paginate: async () => [{ filename: 'docs/guide.md' }],
      rest: {
        pulls: { listFiles: {} },
        issues: {
          get: async ({ issue_number }) => ({ data: issue_number === 7
            ? { state: 'open', body: variant === 'qualified' ? 'Fixes org/repo#9' : variant === 'colon' ? 'Closes: #9' : variant === 'missing-link' ? '' : variant === 'foreign-link' ? 'Fixes other/project#9' : 'Closes #9', labels: base.map(name => ({ name })), assignees: owner }
            : { state: variant === 'closed' ? 'closed' : 'open', pull_request: variant === 'pr-link' ? {} : undefined, labels: issueLabels.map(name => ({ name })), assignees: variant === 'missing-owner' ? [] : owner } }),
          addLabels: async ({ labels }) => additions.push(...labels),
          removeLabel: async ({ name }) => removals.push(name),
        },
      },
    };
    await sync({ github, context: { repo: { owner: 'org', repo: 'repo' }, payload: { pull_request: { number: 7 } } }, core: { info() {} } });
    const eligible = ['eligible', 'qualified', 'colon'].includes(variant);
    assert.equal(additions.includes('needs-human-review'), !eligible);
    assert.equal(removals.includes('ready-for-agent'), !eligible);
  });
}
test('routing is idempotent and never promotes needs-info', () => {
  const first = route([...base, 'safety:ui'], owner);
  assert.deepEqual(route(first, owner), first);
  assert.ok(route(base.map(l => l === 'ready-for-agent' ? 'needs-info' : l), owner).includes('needs-info'));
});
test('closed/wontfix issues are not reopened for agent work', () => {
  assert.ok(!route([...base, 'wontfix'], owner).includes('ready-for-agent'));
});
test('sync reads current state, inherits risk and writes only label deltas', async () => {
  const added = [], removed = [];
  const github = {
    paginate: async () => [{ filename: 'docs/new.md', previous_filename: 'src-app/old.rs' }],
    rest: {
      pulls: { listFiles: {} },
      issues: {
        get: async ({ issue_number }) => ({ data: issue_number === 7
          ? { state: 'open', body: 'Closes #9', labels: base.map(name => ({ name })), assignees: owner }
          : { labels: [{ name: 'safety:access' }] } }),
        addLabels: async ({ labels }) => added.push(...labels),
        removeLabel: async ({ name }) => removed.push(name),
      },
    },
  };
  await sync({ github, context: { repo: { owner: 'org', repo: 'repo' }, payload: { pull_request: { number: 7 } } }, core: { info() {} } });
  assert.ok(added.includes('safety:ui'));
  assert.ok(added.includes('safety:access'));
  assert.ok(added.includes('needs-human-review'));
  assert.ok(removed.includes('ready-for-agent'));
});

for (const issueState of ['open', 'closed']) {
test(`issue ${issueState} event propagates to an existing linked PR`, async () => {
  const writes = [];
  const github = {
    paginate: async (method) => method === 'pulls' ? [{ number: 7 }] : [],
    rest: {
      pulls: { list: 'pulls', listFiles: 'files' },
      issues: {
        get: async ({ owner: repoOwner, repo, issue_number }) => {
          assert.equal(repoOwner, 'org');
          assert.equal(repo, 'repo');
          return { data: {
          state: issue_number === 7 ? 'open' : issueState, assignees: owner,
          body: issue_number === 7 ? 'Fixes https://github.com/org/repo/issues/9' : '',
          labels: (issue_number === 7 ? base : [...base, 'safety:access']).map(name => ({ name })),
          } };
        },
        addLabels: async (args) => writes.push(args),
        removeLabel: async () => {},
      },
    },
  };
  class Context {
    get repo() { return { owner: 'org', repo: 'repo' }; }
  }
  const context = new Context();
  context.payload = { issue: { number: 9 } };
  await sync({ github, context, core: { info() {} } });
  assert.ok(writes.some(w => w.issue_number === 7 && w.labels.includes('needs-human-review')));
  if (issueState === 'closed') assert.ok(writes.every(w => w.issue_number !== 9));
});
}

test('issue closure is subscribed in the actual workflow', () => {
  const { readFileSync } = require('node:fs');
  const workflow = readFileSync(require('node:path').join(__dirname, '../.github/workflows/agent-safety.yml'), 'utf8');
  assert.match(workflow, /issues:\s*\n\s*types: \[[^\]]*\bclosed\b/);
});

for (const status of [404, 403, 500]) {
  test(`unreadable linked issue (${status}) retains path and human holds`, async () => {
    const additions = [], failures = [];
    const github = {
      paginate: async () => [{ filename: 'src-app/main.rs' }],
      rest: {
        pulls: { listFiles: {} },
        issues: {
          get: async ({ issue_number }) => {
            if (issue_number === 999) throw Object.assign(new Error('unavailable'), { status });
            return { data: { state: 'open', body: 'Closes #999', labels: base.map(name => ({ name })), assignees: owner } };
          },
          addLabels: async ({ labels }) => additions.push(...labels),
          removeLabel: async () => {},
        },
      },
    };
    await sync({ github, context: { repo: { owner: 'org', repo: 'repo' }, payload: { pull_request: { number: 7 } } }, core: { info() {}, warning() {}, setFailed(message) { failures.push(message); } } });
    assert.ok(additions.includes('safety:ui'));
    assert.ok(additions.includes('needs-human-review'));
    assert.equal(failures.length, status === 404 ? 0 : 1);
  });
}

test('all four fork-runner guards from b227ca1 remain intact', () => {
  const { readFileSync } = require('node:fs');
  const workflow = readFileSync(require('node:path').join(__dirname, '../.github/workflows/run_tests.yml'), 'utf8');
  const guards = workflow.split('\n').filter(line => line.includes('runs-on:') && line.includes('head.repo.full_name'));
  const expected = "    runs-on: ${{ (github.event_name != 'pull_request' || github.event.pull_request.head.repo.full_name == github.repository) && 'self-hosted' || 'ubuntu-24.04' }}";
  assert.deepEqual(guards, Array(4).fill(expected));
});
