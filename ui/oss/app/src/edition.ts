/**
 * The OSS edition plus one Nandatown layer: GitHub sign-in on /login, so people can try the demo without being given
 * a password. Kept here, in the fork's own app entry, so core syncs never touch it.
 */
import { OSS, type Edition } from '@nasiko/app-core/edition/edition'
import { GithubSignIn } from './GithubSignIn'

export const edition: Edition = {
  ...OSS,
  layers: [{ id: 'nandatown', slots: { loginMethods: GithubSignIn } }],
}
