/**
 * Every string of the CLI guide (`/cli`). Commands come from the upstream CLI reference (fetched 2026-10-22); the
 * installers are this server's own `/install.sh` and `/install.ps1` (ui/shared/ui/public), which install the CLI as
 * `nandatown`. Re-check them when the CLI changes.
 */
export const copy = {
  title: 'Nandatown CLI',
  sub: 'Deploy, chat with and observe your agents from the terminal.',
  newTab: '(opens in a new tab)',
  install: {
    title: 'Install',
    line: 'One command, no account and no administrator rights. Run it again to update.',
    unix: 'macOS and Linux',
    windows: 'Windows (PowerShell)',
    unixCommand: (origin: string) => `curl -fsSL ${origin}/install.sh | bash`,
    windowsCommand: (origin: string) => `irm ${origin}/install.ps1 | iex`,
  },
  tasks: {
    title: 'What do you want to do?',
  },
  start: {
    title: 'Get started',
    install: 'Install the CLI',
    connect: 'Connect to this cluster',
    signIn: 'Sign in with your Nandatown account',
    deploy: 'Deploy an agent from its folder',
  },
  copyCommand: (what: string) => `Copy: ${what}`,
}

/** The "What do you want to do?" cards: one command each (`page` keys the icon). */
export const TASKS = [
  {
    page: 'setup',
    title: 'Set up and sign in',
    line: 'Run the stack locally or connect to a cluster.',
    command: 'nandatown up',
  },
  {
    page: 'coding-agents',
    title: 'Coding agents',
    line: 'Find Claude Code, Codex and others, and track their usage.',
    command: 'nandatown agents discover',
  },
  {
    page: 'create',
    title: 'Create an agent',
    line: 'Scaffold, build and run an A2A agent locally.',
    command: 'nandatown new',
  },
  {
    page: 'operate',
    title: 'Deploy and operate',
    line: 'Upload, deploy, scale and read logs.',
    command: 'nandatown upload',
  },
  {
    page: 'chat',
    title: 'Chat',
    line: 'Talk to an agent or the orchestrator.',
    command: 'nandatown chat --tui',
  },
  {
    page: 'observe',
    title: 'Observe and TokenOps',
    line: 'Sessions, traces and what each run cost.',
    command: 'nandatown observe sessions',
  },
] as const
