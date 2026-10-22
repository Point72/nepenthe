## Table of Contents

- [Table of Contents](#table-of-contents)
- [Step 1: Build from Source](#step-1-build-from-source)
- [Step 2: Configuring Git and GitHub for Development](#step-2-configuring-git-and-github-for-development)
  - [Create your fork](#create-your-fork)
  - [Configure remotes](#configure-remotes)
  - [Authenticating with GitHub](#authenticating-with-github)
- [Guidelines](#guidelines)

## Step 1: Build from Source

Build `nepenthe` from source using the
[build instructions](Build-from-Source).

Then configure `git` and your GitHub account for development.

## Step 2: Configuring Git and GitHub for Development

### Create your fork

Create a personal fork using the "Fork" button at
https://github.com/Point72/nepenthe or the
[fork page](https://github.com/Point72/nepenthe/fork). Select your personal
GitHub account as the owner and click "Create fork".

### Configure remotes

Configure `git` remotes for the main Point72 repository and your fork. See the
[GitHub Docs](https://docs.github.com/en/pull-requests/collaborating-with-pull-requests/working-with-forks/configuring-a-remote-repository-for-a-fork).

### Authenticating with GitHub

Follow GitHub's [SSH setup instructions](https://docs.github.com/en/authentication/connecting-to-github-with-ssh)
to create a key and configure access. If your environment blocks SSH, use a
[personal access token](https://docs.github.com/en/authentication/keeping-your-account-and-data-secure/managing-your-personal-access-tokens)
for Git over HTTPS. GitHub also supports
[passkeys](https://docs.github.com/en/authentication/authenticating-with-a-passkey/about-passkeys)
for signing in through a browser.

## Guidelines

Before submitting a change, run the [lints](Build-from-Source#lint-and-autoformat)
and [tests](Build-from-Source#testing). Squash commits into logical units and
sign off each commit with `git commit -s` to satisfy the
[Developer Certificate of Origin](https://en.wikipedia.org/wiki/Developer_Certificate_of_Origin)
requirement.

Open a [draft pull request](https://docs.github.com/en/pull-requests/collaborating-with-pull-requests/proposing-changes-to-your-work-with-pull-requests/about-pull-requests#draft-pull-requests)
for work in progress, or a regular pull request when it is ready for review.
Review may take a few days. Address requested changes and squash commits as
needed before a maintainer approves and merges the pull request.

Maintainers may close pull requests for non-substantive changes or large changes
that were not discussed beforehand.
