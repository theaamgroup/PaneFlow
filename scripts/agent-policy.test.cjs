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

test('incomplete risky items retain needs-info and their human hold', () => {
  for (const labels of [base.filter(l => l !== 'severity:high'), base]) {
    const result = route([...labels, 'safety:release'], []);
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
  for (const path of ['deny.toml', 'clippy.toml', '.cursor/rules/review.mdc', '.claude/settings.json']) assert.deepEqual(pathRisks([path]), ['safety:release']);
  assert.deepEqual(pathRisks(['skills/paneflow-conductor/SKILL.md']), ['safety:release']);
  assert.deepEqual(pathRisks(['.agents/skills/example/SKILL.md']), ['safety:release']);
  assert.deepEqual(pathRisks(['src-app/src/main.rs', '.github/workflows/test.yml', 'crates/x/src/lib.rs', 'native/a']), ['safety:ui', 'safety:release', 'safety:integration', 'safety:platform-wide']);
  assert.ok(route(base, owner, [], ['safety:access']).includes('needs-human-review'));
  assert.deepEqual(pathRisks(['docs/guide.md']), []);
});

for (const variant of ['eligible', 'qualified', 'colon', 'hidden', 'hidden-unclosed', 'hidden-mixed', 'missing-link', 'foreign-link', 'foreign-redirect', 'pr-link', 'missing-safety', 'missing-owner', 'needs-info', 'closed']) {
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
            ? { state: 'open', body: variant === 'hidden' ? '<!-- Closes #9 -->' : variant === 'hidden-unclosed' ? '<!-- example\nCloses #9' : variant === 'hidden-mixed' ? '<!-- Closes #10 -->\nCloses #9' : variant === 'qualified' ? 'Fixes org/repo#9' : variant === 'colon' ? 'Closes: #9' : variant === 'missing-link' ? '' : variant === 'foreign-link' ? 'Fixes other/project#9' : 'Closes #9', labels: base.map(name => ({ name })), assignees: owner }
            : { repository_url: variant === 'foreign-redirect' ? 'https://api.github.com/repos/other/project' : 'https://api.github.com/repos/org/repo', state: variant === 'closed' ? 'closed' : 'open', pull_request: variant === 'pr-link' ? {} : undefined, labels: issueLabels.map(name => ({ name })), assignees: variant === 'missing-owner' ? [] : owner } }),
          addLabels: async ({ labels }) => additions.push(...labels),
          removeLabel: async ({ name }) => removals.push(name),
        },
      },
    };
    await sync({ github, context: { repo: { owner: 'org', repo: 'repo' }, payload: { pull_request: { number: 7 } } }, core: { info() {} } });
    const eligible = ['eligible', 'qualified', 'colon', 'hidden-mixed'].includes(variant);
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
  assert.match(workflow, /issues:\s*\n\s*types: \[[^\]]*\bdeleted\b/);
  assert.match(workflow, /issues:\s*\n\s*types: \[[^\]]*\btransferred\b/);
});

for (const action of ['deleted', 'transferred']) {
  test(`issue ${action} event routes PRs without fetching the event issue`, async () => {
    const calls = [], additions = [];
    const github = {
      paginate: async method => method === 'pulls' ? [{ number: 7 }] : [],
      rest: {
        pulls: { list: 'pulls', listFiles: 'files' },
        issues: {
          get: async ({ issue_number }) => {
            calls.push(issue_number);
            if (issue_number === 9) throw Object.assign(new Error('missing'), { status: 404 });
            return { data: { state: 'open', body: 'Closes #9', labels: base.map(name => ({ name })), assignees: owner } };
          },
          addLabels: async ({ labels }) => additions.push(...labels),
          removeLabel: async () => {},
        },
      },
    };
    await sync({ github, context: { repo: { owner: 'org', repo: 'repo' }, payload: { action, issue: { number: 9 } } }, core: { info() {}, warning() {} } });
    assert.deepEqual(calls, [7, 9]);
    assert.ok(additions.includes('needs-human-review'));
  });
}

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

for (const state of ['ready-for-agent', 'ready-for-human']) {
  for (const variant of ['complete', 'missing-owner', 'held', 'stale']) {
    test(`explicit ${state} promotion: ${variant}`, async () => {
      const labels = new Set(base.filter(l => l !== 'ready-for-agent'));
      labels.add('needs-info');
      if (variant !== 'stale') labels.add(state);
      if (variant === 'held') labels.add('needs-human-review');
      const github = {
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
      const context = { repo: { owner: 'org', repo: 'repo' }, payload: { action: 'labeled', label: { name: state }, issue: { number: 9 } } };
      await sync({ github, context, core: { info() {} } });
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
  const held = route(['wontfix', 'safety:release'], []);
  assert.ok(held.includes('needs-info'));
  assert.ok(held.includes('needs-human-review'));
});

for (const count of [20, 21, 3000]) {
  test(`${count} closing references have bounded requests and preserve path holds`, async () => {
    const reads = [], added = [], removed = [];
    const github = {
      paginate: async () => [{ filename: 'src-app/main.rs' }],
      rest: {
        pulls: { listFiles: 'files' },
        issues: {
          get: async ({ issue_number }) => {
            reads.push(issue_number);
            if (issue_number !== 7) throw Object.assign(new Error('missing'), { status: 404 });
            return { data: { state: 'open', body: Array.from({ length: count }, (_, i) => `Closes #${i + 100}`).join('\n'), labels: base.map(name => ({ name })), assignees: owner } };
          },
          addLabels: async ({ labels }) => added.push(...labels),
          removeLabel: async ({ name }) => removed.push(name),
        },
      },
    };
    await sync({ github, context: { repo: { owner: 'org', repo: 'repo' }, payload: { pull_request: { number: 7 } } }, core: { info() {}, warning() {} } });
    assert.equal(reads.length, count > 20 ? 1 : 21);
    assert.ok(added.includes('needs-human-review'));
    assert.ok(added.includes('safety:ui'));
    assert.ok(removed.includes('ready-for-agent'));
  });
}

test('throttled lookups stop at the first failure and still attempt the hold', async () => {
  const reads = [], added = [], removed = [], failures = [];
  const github = {
    paginate: async () => [],
    rest: {
      pulls: { listFiles: 'files' },
      issues: {
        get: async ({ issue_number }) => {
          reads.push(issue_number);
          if (issue_number !== 7) throw Object.assign(new Error('throttled'), { status: 429 });
          return { data: { state: 'open', body: 'Closes #9\nCloses #10', labels: base.map(name => ({ name })), assignees: owner } };
        },
        addLabels: async ({ labels }) => added.push(...labels),
        removeLabel: async ({ name }) => removed.push(name),
      },
    },
  };
  await sync({ github, context: { repo: { owner: 'org', repo: 'repo' }, payload: { pull_request: { number: 7 } } }, core: { info() {}, warning() {}, setFailed(message) { failures.push(message); } } });
  assert.deepEqual(reads, [7, 9]);
  assert.equal(failures.length, 1);
  assert.ok(added.includes('needs-human-review'));
  assert.ok(removed.includes('ready-for-agent'));
});
