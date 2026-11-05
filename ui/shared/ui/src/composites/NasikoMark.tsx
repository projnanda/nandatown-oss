import { MARK_DOTS, MARK_VIEWBOX } from './markDots'
import { cn } from '../lib/utils'

/**
 * The Nandatown dot mark, inline so it takes `currentColor` (public/mark-nasiko.svg, the favicon, keeps the fixed
 * yellow). Yellow is the logo only (§6.4): colour it with `text-logo`. The export keeps upstream's name so the
 * importers (sidebar, login, not-found) merge cleanly on every core sync.
 */
export function NasikoMark({ className }: { className?: string }) {
  return (
    <svg viewBox={MARK_VIEWBOX} fill="currentColor" aria-hidden className={className}>
      {MARK_DOTS.map((d) => (
        <circle key={`${d.cx},${d.cy}`} {...d} />
      ))}
    </svg>
  )
}

/**
 * The Nandatown lockup: the mark (`text-logo`) beside the "Nandatown" wordmark (`currentColor`). Size it by height
 * (`h-6 w-auto`), as with upstream's lockup; the mark is square and the wordmark scales with the height.
 */
export function NasikoLockup({ className }: { className?: string }) {
  return (
    <span role="img" aria-label="Nandatown" className={cn('inline-flex items-center gap-[0.35em] text-[1.1em]', className)}>
      <NasikoMark className="aspect-square h-full w-auto text-logo" />
      <span aria-hidden className="font-semibold leading-none tracking-tight">
        Nandatown
      </span>
    </span>
  )
}
