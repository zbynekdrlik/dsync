## dsync Context
- Act as senior rust,windows, hw, clock skilled developr.
- Use tdd approach and ensure that all code has 100% tests coverage.
- **CI/CD Verification:** ALWAYS wait until GitHub Actions CI/CD pipeline has successfully finished (Green Checkmark) before telling the user to update or run commands. Do not assume success. Monitor `gh run view` until completion.
- **Deployment:** Never deploy to a node on your own. The rules in `docs/HANDOVER.md` apply: the camera rig belongs to camera-box, and requests come through dstack. Site facts are in `~/.config/dsync/TARGETS.md`.
- **Local Verification:** Prioritize local `cargo build`, `cargo test`, and running the binary locally to verify changes and speed up the feedback loop before pushing to GitHub or deploying remotely.
