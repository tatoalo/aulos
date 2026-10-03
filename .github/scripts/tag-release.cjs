module.exports = async ({ github, context, core }) => {
  const pr = context.payload.pull_request;
  if (!pr?.merged || pr.base.ref !== 'main' || !pr.labels.some(({ name }) => name === 'release')) return;

  const base = `v${pr.merged_at.slice(0, 10).replaceAll('-', '.')}`;
  for (let suffix = 0; ; suffix++) {
    const tag = suffix ? `${base}.${suffix}` : base;
    try {
      const { data } = await github.rest.git.getRef({ ...context.repo, ref: `tags/${tag}` });
      if (data.object.sha === pr.merge_commit_sha) {
        core.info(`${tag} already points to the merged commit`);
        return tag;
      }
    } catch (error) {
      if (error.status !== 404) throw error;
      await github.rest.git.createRef({
        ...context.repo,
        ref: `refs/tags/${tag}`,
        sha: pr.merge_commit_sha,
      });
      core.info(`Created ${tag} at ${pr.merge_commit_sha}`);
      return tag;
    }
  }
};
