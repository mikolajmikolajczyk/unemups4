---
id: TASK-186
title: >-
  gnm/gpu: offscreen RT images ignore the guest's COMP_SWAP — created RGBA
  regardless, and the render pass format does not match the image
status: To Do
assignee: []
created_date: '2026-07-20 17:14'
labels:
  - gpu
  - gnm
  - rt
  - format
  - portability
dependencies: []
priority: medium
ordinal: 190000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
register_render_target in crates/gnm/src/exec.rs passes a hardcoded ColorFormat::R8G8B8A8Unorm to CreateRenderTarget, discarding the channel order already decoded from CB_COLOR0_INFO.COMP_SWAP into TargetDesc.format (task-154: STD -> RGBA, ALT -> BGRA). Celeste programs ALT, so every one of its offscreen targets is created as an RGBA image while the guest believes it is BGRA. Separately, create_rt_target in crates/gpu/src/backend.rs builds the offscreen render pass with the hardcoded EMBEDDED_TARGET_FORMAT while the image itself comes from vk_color_format(format) — so the render-pass format and the image format can disagree. The current driver tolerates that; MoltenVK/Metal is not required to, which makes it a portability risk against the Bloodborne-on-macOS goal.\n\nWhy this has not bitten yet: while an RT stays inside our pipeline — a shader exports into it, another shader samples it — RGBA-vs-BGRA is a consistent relabelling that cancels out. It only becomes observable at boundaries where guest byte order matters: the RT readback writing into guest memory, the guest CPU touching those bytes, and a T# whose dst_sel differs from what the surface was written with.\n\nThe fix is to pass target.format through instead of the constant, in BOTH places. In Vulkan the format declares memory order and the fragment shader always writes semantic .rgba, so the driver places the bytes — switching an image to B8G8R8A8_UNORM needs NO shader change and no swizzle. This is not 'add a channel swap'; it is 'stop telling the driver something untrue about the surface'.\n\nDoing this also closes task-181's one remaining hole BY CONSTRUCTION rather than by adding a detector: if the image format equals the guest's format, the readback is a straight copy and cannot swap channels. task-181 currently records that case as 'not handled, not detected'.
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [ ] #1 register_render_target passes the derived TargetDesc.format instead of a hardcoded ColorFormat
- [ ] #2 The offscreen render pass is created with the same format as the image it renders into — no EMBEDDED_TARGET_FORMAT/vk_color_format disagreement
- [ ] #3 Audited and recorded: no SECOND channel swap is applied anywhere downstream (swap_rb, the present path, the RT-as-texture sampling path). task-175 was two swap bugs where each fix ALONE looked like a regression — verify, do not assume
- [ ] #4 task-181's readback confidence claim is updated: the channel-order case is closed by construction, not merely detected
- [ ] #5 Celeste's splash, title and menu render unchanged — maintainer's live oracle, since this change touches the picture
- [ ] #6 build + cargo test + clippy clean
<!-- AC:END -->
