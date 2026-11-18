# Lancer source provenance

Source: https://github.com/raylee-ai/lancer

Revision: `0ccc4584fc0753fa128e71464814d1ed09dd1dbc`

`src/` is copied without changes from `packages/light-ui/src/`, retaining its
original `LICENSE` and `NOTICE`. ARIS uses its actual `createLightLayer` renderer,
flight geometry, arrival effects, reduced-motion handling and disposal logic.

The ARIS `WorkspaceTrajectory` layout, node glyphs and CSS adapt the structural
patterns in `apps/desktop/src/features/conversation/lancer-process/`:
`process-surface.tsx`, `process-tree.tsx`, `process-glyph.tsx` and
`process-surface.css`, under the same MIT license. ARIS supplies its own projection
of real team/session records; no Lancer replay fixture or sample results are used.

`model-brand-marks.ts` is copied from Lancer's `apps/desktop/src/ui/` and contains
Lobe Icons v1.99.0 (MIT), with `LOBE-ICONS-LICENSE` retained here. Provider names
and marks belong to their respective owners and do not imply endorsement.
