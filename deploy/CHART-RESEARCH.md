# Papertrade chart study

Inspected the public browser artifacts served by `https://exchange.papertrade.xyz/trade/btc` on 2026-11-06 using the installed REA reverse-engineer-anything skill, its static JavaScript CLI, and gstack browser observation.

REA Evidence: `ev_98b7b8b61765fac6d07e00733c8e7e2d0122bbcf7ac1ccffb4503a70116a8ad1`. Complete output, source digests, and captured bundles are retained locally under `.gstack/rea-papertrade/`. Four selected chunks were analyzed: `TradeChart-yOY8N_Z-.js`, `chart-live-motion-BnTI8ozC.js`, `chart-history-BsyjblFC.js`, and `firehose-Cq-L2wCt.js`.

Observed in shipped JavaScript:

- A persistent Canvas 2D surface sized for device pixel ratio.
- A requestAnimationFrame drawing scheduler, separate from market subscriptions.
- Presentation time advances using the animation clock between data arrivals.
- The final sample's position and value are eased with an exponential response around 120 ms, snapping after long interruptions.
- Repainting is skipped for subpixel changes (approximately 0.1 display pixels).
- Tick history has adaptive resolution/retention rather than unlimited dense data.
- Reduced-motion preferences, hidden-tab subscriptions, and resize observers participate in rendering.

Applied to Optionality: persistent Canvas drawing, a monotonic display clock, frame-based price smoothing, pixel-based repaint thresholds, no overlapping tick requests, hidden-tab pause, reduced-motion snapping, and a single combined historical/live price path. Quote and order values remain unsmoothed.

Limits: this is selected-artifact static evidence plus browser observation, not original source recovery or complete backend analysis. Uncaptured imports remain unresolved in REA's graph. The exchange's exact server tick source, latency guarantees, and full rendering toolkit were not established. No Papertrade source code or branded UI is shipped in Optionality.

## Landing media

The landing uses the existing v13 film, its captions and credits, and a short muted loop derived from the US National Archives CBOT footage already included in the film assets. The full film is loaded only when requested; playback uses byte ranges for seeking. Keep `demo/media/credits.txt` available wherever the film is published.

`demo/media/*.mp4` files are excluded from git because the original video files are maintained in the film directory. Deploy them alongside the page. The build script deliberately preserves these files. The hosted app's environment was confirmed as `sandbox` via `/config`; the landing distinguishes that hosted demo from separate testnet deployments.

The liquid-glass template validator flags two intentional product adaptations: there is no email-capture form because no signup endpoint exists, and the user-controlled launch film retains sound rather than carrying a `muted` attribute. The decorative background video is muted and inline. These template checks do not distinguish a foreground film from a background video.
