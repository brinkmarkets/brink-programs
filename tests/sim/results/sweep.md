# Maths sweep

Elapsed 3.5 s.

| Check | Evaluated | Failures | Note |
|---|---:|---:|---|
| quote within reference +/- (model + demand cap + term), correct side | 40005504 | 0 | 0 quote errors (bad utilisation inputs), fixed range [-1400, 31400] |
| pay quote >= receive quote | 20002752 pairs | 0 | |
| pnl bounded by collateral | 29645 | 0 | 120 overflow errors (u64::MAX-scale inputs) |
| pnl antisymmetric in the rate difference | 29645 | 0 | |
