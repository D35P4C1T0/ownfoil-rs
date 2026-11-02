async function weekly({github, context, core}) {
  const repo = context.repo;
  // A published tag can belong to a failed artifact or image run.
  const {data} = await github.rest.actions.listWorkflowRuns({
    ...repo, workflow_id: 'weekly.yml', branch: 'main', status: 'success', per_page: 1,
  });
  let previous = data.workflow_runs[0]?.head_sha;
  if (!previous) {
    const release = await latestRelease(github, repo);
    if (release) {
      const {data: commit} = await github.rest.repos.getCommit({...repo, ref: release.tag_name});
      previous = commit.sha;
    }
  }
  const changed = previous !== context.sha;
  core.setOutput('build', changed);
  if (!changed) {
    core.info('No new commits on main; skipping weekly artifacts and images.');
    return;
  }
  const tag = `weekly-${new Date().toISOString().slice(0, 10).replaceAll('-', '')}`;
  try {
    const {data: existing} = await github.rest.repos.getCommit({...repo, ref: tag});
    if (existing.sha !== context.sha) throw new Error(`${tag} already points to another commit; retry on a later UTC date.`);
  } catch (error) {
    if (error.status !== 404) throw error;
  }
  core.setOutput('ref', context.sha);
  core.setOutput('tag', tag);
}

async function latestRelease(github, repo) {
  try {
    return (await github.rest.repos.getLatestRelease(repo)).data;
  } catch (error) {
    if (error.status !== 404) throw error;
    return null;
  }
}

async function imageTags({github, context, core, version, image}) {
  const tags = [`${image}:${version}`];
  if (version !== 'weekly') {
    const latest = await latestRelease(github, context.repo);
    if (latest?.tag_name === version) tags.push(`${image}:latest`);
  }
  core.setOutput('tags', tags.join('\n'));
}

module.exports = {weekly, imageTags};
