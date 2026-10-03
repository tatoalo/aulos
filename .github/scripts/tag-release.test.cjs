const { test } = require('node:test');
const assert = require('node:assert/strict');
const tagRelease = require('./tag-release.cjs');

function harness(overrides = {}, refs = new Map()) {
  const created = [];
  const args = {
    context: {
      repo: { owner: 'tatoalo', repo: 'aulos' },
      payload: { pull_request: {
        merged: true, base: { ref: 'main' }, labels: [{ name: 'release' }],
        merged_at: '2026-10-03T14:00:00Z', merge_commit_sha: 'merged-sha',
        head: { sha: 'unmerged-sha' }, ...overrides,
      } },
    },
    core: { info() {} },
    github: { rest: { git: {
      async getRef({ ref }) {
        if (!refs.has(ref)) throw Object.assign(new Error('Not found'), { status: 404 });
        return { data: { object: { sha: refs.get(ref) } } };
      },
      async createRef(request) {
        created.push(request);
        refs.set(request.ref.replace(/^refs\//, ''), request.sha);
      },
    } } },
  };
  return { args, created };
}

test('tags the merged commit and a rerun leaves the tag untouched', async () => {
  const { args, created } = harness();
  assert.equal(await tagRelease(args), 'v2026.10.03');
  assert.equal(await tagRelease(args), 'v2026.10.03');
  assert.deepEqual(created, [{
    owner: 'tatoalo', repo: 'aulos', ref: 'refs/tags/v2026.10.03', sha: 'merged-sha',
  }]);
});

test('another release on the same date gets a suffix without moving existing tags', async () => {
  const { args, created } = harness({}, new Map([
    ['tags/v2026.10.03', 'previous-sha'], ['tags/v2026.10.03.1', 'another-sha'],
  ]));
  assert.equal(await tagRelease(args), 'v2026.10.03.2');
  assert.equal(await tagRelease(args), 'v2026.10.03.2');
  assert.equal(created.length, 1);
  assert.equal(created[0].sha, 'merged-sha');
});

for (const [name, overrides] of [
  ['closed without merging', { merged: false }],
  ['merged without the release label', { labels: [] }],
  ['merged to another branch', { base: { ref: 'other' } }],
]) {
  test(name + ' does not create a release', async () => {
    const { args, created } = harness(overrides);
    assert.equal(await tagRelease(args), undefined);
    assert.equal(created.length, 0);
  });
}

test('an API failure is surfaced without creating a tag', async () => {
  const { args, created } = harness();
  args.github.rest.git.getRef = async () => { throw Object.assign(new Error('Forbidden'), { status: 403 }); };
  await assert.rejects(tagRelease(args), /Forbidden/);
  assert.equal(created.length, 0);
});
