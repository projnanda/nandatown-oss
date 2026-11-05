/**
 * /login's "Continue with GitHub" (the `loginMethods` slot), so people can try the demo without being handed a
 * password. Shown only once `/api/auth/github/status` says the server has a GitHub OAuth app. The server runs the
 * flow (server/src/github.rs): `login-user` hands back GitHub's authorize URL, the callback finds or creates the user,
 * sets the session cookie and sends the browser to `/login?signin=external`, where /login finishes the sign-in.
 */
import { useMutation, useQuery } from '@tanstack/react-query'
import type { LoginMethodsProps } from '@nasiko/app-core/edition/edition'
import { Button } from '@nasiko/ui/primitives/button'
import { FieldError } from '@nasiko/ui/primitives/field'
import { Separator } from '@nasiko/ui/primitives/separator'
import { Spinner } from '@nasiko/ui/primitives/spinner'
import { apiFetch } from '@nasiko/data-access/client'

const copy = {
  or: 'or',
  continue: 'Continue with GitHub',
  failed: "Couldn't start GitHub sign-in. Try again.",
  errors: {
    personal_email_not_allowed: 'This workspace only accepts work email addresses.',
  } as Record<string, string>,
}

/** Flat, not the `{data}` envelope (github.rs `github_login_configured` / `github_login_user`). */
interface GithubStatus {
  configured: boolean
}
interface GithubLoginUrl {
  auth_url: string
}

export function GithubSignIn({ onStart, error }: LoginMethodsProps) {
  const status = useQuery({
    queryKey: ['auth', 'github-status'],
    queryFn: ({ signal }) => apiFetch<GithubStatus>('/api/auth/github/status', { signal }),
    // A probe: a failed one just means no button.
    retry: false,
  })
  const start = useMutation({
    mutationFn: () => apiFetch<GithubLoginUrl>('/api/auth/github/login-user'),
    onSuccess: ({ auth_url }) => {
      onStart()
      window.location.assign(auth_url)
    },
  })
  const message = start.isError ? copy.failed : error ? (copy.errors[error] ?? copy.failed) : null
  if (!status.data?.configured)
    return message ? <FieldError className="mt-3 text-xs">{message}</FieldError> : null
  // Stays busy after success too: the page is on its way to GitHub.
  const busy = start.isPending || start.isSuccess
  return (
    <div className="mt-3 flex flex-col gap-5.5">
      <div className="flex items-center gap-3 text-xs text-muted-foreground">
        <Separator className="flex-1" />
        {copy.or}
        <Separator className="flex-1" />
      </div>
      <Button
        type="button"
        variant="outline"
        size="lg"
        className="h-13 rounded-[10px] text-[15px]"
        disabled={busy}
        onClick={() => start.mutate()}
      >
        {busy ? <Spinner aria-hidden /> : <GithubMark />}
        {copy.continue}
      </Button>
      {message ? <FieldError className="text-xs">{message}</FieldError> : null}
    </div>
  )
}

/** GitHub's mark (lucide no longer ships brand icons). */
function GithubMark() {
  return (
    <svg viewBox="0 0 16 16" fill="currentColor" aria-hidden className="size-4">
      <path d="M8 0C3.58 0 0 3.58 0 8c0 3.54 2.29 6.53 5.47 7.59.4.07.55-.17.55-.38 0-.19-.01-.82-.01-1.49-2.01.37-2.53-.49-2.69-.94-.09-.23-.48-.94-.82-1.13-.28-.15-.68-.52-.01-.53.63-.01 1.08.58 1.23.82.72 1.21 1.87.87 2.33.66.07-.52.28-.87.51-1.07-1.78-.2-3.64-.89-3.64-3.95 0-.87.31-1.59.82-2.15-.08-.2-.36-1.02.08-2.12 0 0 .67-.21 2.2.82.64-.18 1.32-.27 2-.27.68 0 1.36.09 2 .27 1.53-1.04 2.2-.82 2.2-.82.44 1.1.16 1.92.08 2.12.51.56.82 1.27.82 2.15 0 3.07-1.87 3.75-3.65 3.95.29.25.54.73.54 1.48 0 1.07-.01 1.93-.01 2.2 0 .21.15.46.55.38A8.013 8.013 0 0016 8c0-4.42-3.58-8-8-8z" />
    </svg>
  )
}
