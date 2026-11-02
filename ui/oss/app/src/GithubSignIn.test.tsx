/**
 * Nandatown's GitHub sign-in on /login: the button follows `/api/auth/github/status`, a click marks the tab and leaves
 * for GitHub's authorize URL, and a failed start or return is explained.
 */
import { QueryClient, QueryClientProvider } from '@tanstack/react-query'
import { render, screen, waitFor } from '@testing-library/react'
import userEvent from '@testing-library/user-event'
import { http, HttpResponse } from 'msw'
import { afterEach, describe, expect, it, vi } from 'vitest'
import { server } from '@nasiko/testing/setup'
import { GithubSignIn } from './GithubSignIn'

const AUTH_URL = 'https://github.com/login/oauth/authorize?client_id=x&state=y'

function renderButton(props: { error?: string; onStart?: () => void } = {}) {
  const client = new QueryClient({ defaultOptions: { queries: { retry: false } } })
  return render(
    <QueryClientProvider client={client}>
      <GithubSignIn returnTo="/login?signin=external" onStart={props.onStart ?? vi.fn()} error={props.error} />
    </QueryClientProvider>,
  )
}

const configured = (on: boolean) =>
  server.use(http.get('*/api/auth/github/status', () => HttpResponse.json({ configured: on })))

afterEach(() => vi.unstubAllGlobals())

describe('GitHub sign-in (Nandatown)', () => {
  it('offers no button until the server has a GitHub OAuth app', async () => {
    configured(false)
    const { container } = renderButton()
    await new Promise((r) => setTimeout(r, 50))
    expect(screen.queryByRole('button', { name: 'Continue with GitHub' })).not.toBeInTheDocument()
    expect(container).toBeEmptyDOMElement()
  })

  it('marks the tab and leaves for GitHub', async () => {
    configured(true)
    server.use(http.get('*/api/auth/github/login-user', () => HttpResponse.json({ auth_url: AUTH_URL })))
    const assign = vi.fn()
    vi.stubGlobal('location', { ...window.location, assign })
    const onStart = vi.fn()
    renderButton({ onStart })
    await userEvent.click(await screen.findByRole('button', { name: 'Continue with GitHub' }))
    await waitFor(() => expect(assign).toHaveBeenCalledWith(AUTH_URL))
    expect(onStart).toHaveBeenCalledOnce()
  })

  it('says so when the sign-in cannot start', async () => {
    configured(true)
    server.use(http.get('*/api/auth/github/login-user', () => HttpResponse.json({}, { status: 503 })))
    renderButton()
    await userEvent.click(await screen.findByRole('button', { name: 'Continue with GitHub' }))
    expect(await screen.findByText("Couldn't start GitHub sign-in. Try again.")).toBeInTheDocument()
  })

  it('explains a rejected return', async () => {
    configured(true)
    renderButton({ error: 'personal_email_not_allowed' })
    expect(await screen.findByText('This workspace only accepts work email addresses.')).toBeInTheDocument()
  })
})
