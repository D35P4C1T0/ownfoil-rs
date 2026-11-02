const {test} = require('node:test');
const assert = require('node:assert/strict');
const {weekly, imageTags} = require('./release-plan.cjs');

function fixture(previous, stable = 'v0.4.0', stableCommit = 'stable-sha', existingWeekly) {
  const output = {};
  return {
    output,
    context: {repo: {owner: 'owner', repo: 'repo'}, sha: 'main-sha'},
    core: {setOutput: (name, value) => { output[name] = value; }, info: () => {}},
    github: {rest: {
      actions: {listWorkflowRuns: async options => {
        assert.equal(options.branch, 'main');
        assert.equal(options.status, 'success');
        return {data: {workflow_runs: previous ? [{head_sha: previous}] : []}};
      }},
      repos: {
        getLatestRelease: async () => {
          if (!stable) throw Object.assign(new Error('No releases'), {status: 404});
          return {data: {tag_name: stable}};
        },
        getCommit: async ({ref}) => {
          if (ref.startsWith('weekly-')) {
            if (!existingWeekly) throw Object.assign(new Error('No weekly tag'), {status: 404});
            return {data: {sha: existingWeekly}};
          }
          assert.equal(ref, stable);
          return {data: {sha: stableCommit}};
        },
      },
    }},
  };
}

test('unchanged main skips all publishing', async () => {
  const run = fixture('main-sha');
  await weekly(run);
  assert.deepEqual(run.output, {build: false});
});

test('new commits get a frozen source and dated weekly tag', async t => {
  t.mock.timers.enable({apis: ['Date'], now: new Date('2026-11-01T03:17:00Z')});
  const run = fixture('older-successful-sha');
  await weekly(run);
  assert.deepEqual(run.output, {build: true, ref: 'main-sha', tag: 'weekly-20261101'});
});

test('first weekly skips main already released as stable', async () => {
  const run = fixture(null, 'v0.4.0', 'main-sha');
  await weekly(run);
  assert.deepEqual(run.output, {build: false});
});

test('first weekly builds unreleased commits or a repo without releases', async () => {
  for (const stable of ['v0.4.0', null]) {
    const run = fixture(null, stable);
    await weekly(run);
    assert.equal(run.output.build, true);
    assert.equal(run.output.ref, 'main-sha');
  }
});

test('failed runs can retry the same commit without moving a dated tag', async () => {
  const retry = fixture('older-successful-sha', 'v0.4.0', 'stable-sha', 'main-sha');
  await weekly(retry);
  assert.equal(retry.output.ref, 'main-sha');
  const collision = fixture('older-successful-sha', 'v0.4.0', 'stable-sha', 'different-sha');
  await assert.rejects(weekly(collision), /already points to another commit/);
});

test('API failures stop publishing rather than pretending no release exists', async () => {
  const run = fixture(null);
  run.github.rest.repos.getLatestRelease = async () => {
    throw Object.assign(new Error('Forbidden'), {status: 403});
  };
  await assert.rejects(weekly(run), /Forbidden/);
  assert.deepEqual(run.output, {});
});

test('weekly, old stable and prerelease images cannot move latest', async () => {
  for (const [version, expected] of [
    ['weekly', 'weekly'], ['v0.3.1', 'v0.3.1'], ['v0.5.0-rc.1', 'v0.5.0-rc.1'],
    ['v0.4.0', 'v0.4.0\nghcr.io/owner/repo:latest'],
  ]) {
    const run = fixture(null);
    await imageTags({...run, version, image: 'ghcr.io/owner/repo'});
    assert.equal(run.output.tags, `ghcr.io/owner/repo:${expected}`);
  }
});
