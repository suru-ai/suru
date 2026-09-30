# Require Model evidence for Subagent Cost estimates

_The Cost Coverage rule that a whole-tree amount includes descendants is bounded by ADR-0039: it covers only the native Subagents its Provider ran, never a brokered Subagent._

A Subagent's actual Model must come from Provider evidence rather than inheriting its parent's identity, and Suru leaves its estimated Cost absent until the Model and its price are known. Provider-reported Cost remains usable independently of Model identity; this replaces Codex's parent-rate fallback because an Estimated Cost Basis describes the price source, not permission to assume which Model consumed the tokens.

This extends ADR 0016's unknown-versus-zero rule while retaining its prohibition on restating historical Costs against later prices. This work observes Provider-selected Models; configuring which Models Subagents use is deferred.

## Cost Coverage

Costs must state which work they cover: a Provider's whole-tree amount includes descendants and cannot be added to their individual Costs again. Where some work remains uncovered, totals retain known amounts and are marked partial; unknown child Costs alone do not make a Provider's whole-tree amount partial. This extends ADR 0016's Turn-based accounting to allow Provider-reported amounts whose coverage spans the Session tree, because assigning those amounts to the parent's own work would invent a breakdown the Provider did not supply.

## Model changes during delegated work

The child presents its latest confirmed Model. If a Model change makes cumulative Usage attribution ambiguous, Suru retains any previously established Cost as partial and stops estimating additional Cost for that child's run, even if the Model later switches back; tokens continue to be recorded. Per-call pricing with an identified Model and Provider-reported amounts can continue independently. Exact attribution across ambiguous Model changes and a Model-history UI are deferred: Codex's settings and cumulative Usage notifications do not establish the boundary needed to price each Model's work reliably.
