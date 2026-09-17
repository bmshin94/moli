//! Measure cells at resolved column widths and pass solved rows to Grid.
use super::*;
use rows::{RowConstraint, RowspanConstraint};

#[derive(Clone, Copy)]
pub(super) struct CellBlockLayout {
    pub size: f32,
    pub natural_size: f32,
    pub definite: bool,
    pub baseline: Option<f32>,
}

pub(super) fn set_block<T>(mode: WritingMode, size: &mut Size<T>, value: T) {
    if mode.is_horizontal() {
        size.height = value;
    } else {
        size.width = value;
    }
}

pub(super) fn clear_cell_block_sizing(style: &mut Style<Atom>, mode: WritingMode) {
    set_block(mode, &mut style.size, Dimension::auto());
    set_block(mode, &mut style.min_size, Dimension::auto());
    set_block(mode, &mut style.max_size, Dimension::auto());
}

fn fixed(dimension: Dimension) -> Option<f32> {
    (dimension.tag() == taffy::CompactLength::LENGTH_TAG).then(|| dimension.value().max(0.0))
}

fn percent(dimension: Dimension) -> Option<f32> {
    (dimension.tag() == taffy::CompactLength::PERCENT_TAG).then(|| dimension.value().max(0.0))
}

fn block_sum(mode: WritingMode, rect: Rect<f32>) -> f32 {
    if mode.is_horizontal() {
        rect.top + rect.bottom
    } else {
        rect.left + rect.right
    }
}

impl TableContext {
    pub(super) fn align_row_baselines<N>(
        &self,
        world: &mut LayoutWorld<N>,
        output: &mut LayoutOutput,
    ) where
        N: Copy + Debug + Eq + Hash,
    {
        let mut baselines: Vec<Option<f32>> = vec![None; self.rows.len()];
        for cell in &self.cells {
            if let Some(baseline) = cell.block_layout.and_then(|layout| layout.baseline) {
                baselines[cell.row] = Some(baselines[cell.row].unwrap_or(0.0).max(baseline));
            }
        }
        for cell in &self.cells {
            if let Some(baseline) = cell.block_layout.and_then(|layout| layout.baseline) {
                let offset = baselines[cell.row].unwrap_or(baseline) - baseline;
                if offset > 0.0 {
                    shift_cell_contents(world, cell.id, offset);
                }
            }
        }
        if let Some(detailed) = &self.detailed {
            let padding = self
                .style
                .padding
                .resolve_or_zero(Some(output.size.width), resolve_stylo_calc_value);
            let border = self
                .style
                .border
                .resolve_or_zero(Some(output.size.width), resolve_stylo_calc_value);
            let starts = track_starts(
                padding.top + border.top,
                &detailed.rows.sizes,
                &detailed.rows.gutters,
            );
            if let Some(Some(baseline)) = baselines.first() {
                output.first_baselines.y = Some(starts[self.rows[0].grid_index] + baseline);
            }
            if let Some(Some(baseline)) = baselines.last() {
                output.last_baselines.y =
                    Some(starts[self.rows.last().unwrap().grid_index] + baseline);
            }
        }
    }

    pub(super) fn resolve_row_tracks<N>(
        &mut self,
        world: &mut LayoutWorld<N>,
        inputs: LayoutInput,
    ) -> LayoutInput
    where
        N: Copy + Debug + Eq + Hash,
    {
        let mode = self.writing_mode;
        // Inline-only intrinsic probes do not need a second cell measurement.
        if inputs.run_mode == RunMode::ComputeSize
            && inputs.axis == RequestedAxis::from(mode.inline_axis())
        {
            return inputs;
        }
        let mut space = inputs.constraint_space(mode);
        let percentage_basis = space.margin_padding_percentage_basis();
        let padding = self
            .style
            .padding
            .resolve_or_zero(percentage_basis, resolve_stylo_calc_value);
        let border = self
            .style
            .border
            .resolve_or_zero(percentage_basis, resolve_stylo_calc_value);
        let insets = block_sum(mode, padding) + block_sum(mode, border);
        let preferred = mode.to_logical(self.style.size).block_size;
        let adjustment = if self.style.box_sizing == taffy::BoxSizing::ContentBox {
            // Outer cell spacing is projected as Grid padding, but belongs
            // inside the CSS table's content box.
            insets - 2.0 * self.block_border_spacing
        } else {
            0.0
        };
        let resolve = |d: Dimension| {
            d.maybe_resolve(
                space.percentage_resolution_size.block_size,
                resolve_stylo_calc_value,
            )
            .map(|v| v + adjustment)
        };
        let authored = inputs.sizing_mode == SizingMode::InherentSize;
        let target = space
            .known_size
            .block_size
            .or_else(|| authored.then(|| resolve(preferred)).flatten());
        let min = authored
            .then(|| resolve(mode.to_logical(self.style.min_size).block_size))
            .flatten();
        let max = authored
            .then(|| resolve(mode.to_logical(self.style.max_size).block_size))
            .flatten();
        let target = target
            .map(|v| v.min(max.unwrap_or(f32::INFINITY)))
            .unwrap_or(0.0)
            .max(min.unwrap_or(0.0));
        let mut rows: Vec<_> = self
            .rows
            .iter()
            .map(|row| {
                let dimension = mode
                    .to_logical(world.boxes[row.id.index()].style.taffy.size)
                    .block_size;
                RowConstraint {
                    size: fixed(dimension).unwrap_or(0.0),
                    percent: percent(dimension),
                    constrained: fixed(dimension).is_some() || percent(dimension).is_some(),
                    ..Default::default()
                }
            })
            .collect();
        let mut spans = Vec::new();
        for index in 0..self.cells.len() {
            let cell = &self.cells[index];
            let authored_style = &world.boxes[cell.id.index()].style.taffy;
            let dimension = mode.to_logical(authored_style.size).block_size;
            let cell_padding = authored_style
                .padding
                .resolve_or_zero(space.known_size.inline_size, resolve_stylo_calc_value);
            let cell_border = authored_style
                .border
                .resolve_or_zero(space.known_size.inline_size, resolve_stylo_calc_value);
            let cell_insets = block_sum(mode, cell_padding) + block_sum(mode, cell_border);
            let css_size = outer_fixed_size(dimension, cell_insets, authored_style.box_sizing);
            let cell_percent = percent(dimension);
            let baseline_aligned = matches!(
                world.boxes[cell.id.index()].style.vertical_align().kind,
                LayoutInlineAlignment::Baseline
            ) && authored_style.align_content.is_none();
            let inline = self.column_sizes[cell.column..cell.column + cell.column_span]
                .iter()
                .sum::<f32>()
                + self.inline_border_spacing * cell.column_span.saturating_sub(1) as f32;
            let measure_inputs = LayoutInput {
                known_dimensions: mode.to_physical(LogicalSize {
                    inline_size: Some(inline),
                    block_size: None,
                }),
                definite_dimensions: mode.to_physical(LogicalSize {
                    inline_size: Some(inline),
                    block_size: None,
                }),
                parent_size: mode.to_physical(LogicalSize {
                    inline_size: space.known_size.inline_size,
                    block_size: None,
                }),
                parent_writing_mode: mode,
                available_space: mode.to_physical(LogicalSize {
                    inline_size: AvailableSpace::Definite(inline),
                    block_size: AvailableSpace::MaxContent,
                }),
                run_mode: RunMode::ComputeSize,
                sizing_mode: SizingMode::InherentSize,
                sizing_purpose: SizingPurpose::Layout,
                axis: RequestedAxis::Both,
                block_auto_behavior: AutoSizeBehavior::FitContent,
                vertical_margins_are_collapsible: Line::FALSE,
            };
            // Percentage padding is relative to the table, not the cell's
            // Grid area. Freeze that basis across both measurement and layout.
            self.cells[index].style.padding = cell_padding.map(style_helpers::length);
            let restricted = !preferred.is_auto() || fixed(dimension).is_some();
            let cell_id = self.cells[index].id;
            let restored =
                restrict_scrollable_percentage_children(world, cell_id, mode, restricted);
            let mut wrapper = TableTreeWrapper {
                world,
                context: self,
            };
            let output = wrapper.with_grid_cell_style(index, |world, cell| {
                world.compute_child_layout(cell.to_taffy(), measure_inputs)
            });
            for (id, style) in restored {
                world.boxes[id.index()].style.taffy = style;
                world.cache_clear(id.to_taffy());
            }
            let natural = mode.to_logical(output.size).block_size;
            let cell = &mut self.cells[index];
            let baseline = if mode.is_horizontal() {
                output.first_baselines.y
            } else {
                output.first_baselines.x
            };
            let end_inset = if mode.is_horizontal() {
                cell_padding.bottom + cell_border.bottom
            } else {
                cell_padding.left + cell_border.left
            };
            let baseline = (baseline_aligned && !world.boxes[cell.id.index()].children.is_empty())
                .then(|| baseline.unwrap_or((natural - end_inset).max(0.0)));
            cell.block_layout = Some(CellBlockLayout {
                size: 0.0,
                natural_size: natural,
                definite: fixed(dimension).is_some(),
                baseline,
            });
            let row = &mut rows[cell.row];
            if baseline_aligned && !world.boxes[cell.id.index()].children.is_empty() {
                row.ascent = Some(row.ascent.unwrap_or(0.0).max(baseline.unwrap_or(0.0)));
                if cell.row_span == 1 {
                    row.descent = row.descent.max(natural - baseline.unwrap_or(0.0));
                }
            }
            let minimum = natural.max(css_size.unwrap_or(0.0));
            if cell.row_span == 1 {
                row.size = row.size.max(minimum);
                row.constrained |= css_size.is_some() || cell_percent.is_some();
                if let Some(p) = cell_percent {
                    row.percent = Some(row.percent.unwrap_or(0.0).max(p));
                }
            } else {
                row.has_rowspan_start = true;
                spans.push(RowspanConstraint {
                    rows: cell.row..cell.row + cell.row_span,
                    size: minimum,
                });
            }
        }
        rows::resolve_minimums(
            &mut rows,
            &mut self.sections,
            &mut spans,
            self.block_border_spacing,
        );
        let nonempty_sections = self
            .sections
            .iter()
            .filter(|section| !section.rows.is_empty())
            .count();
        let minimum = self
            .sections
            .iter()
            .map(|section| section.size)
            .sum::<f32>()
            + nonempty_sections.saturating_sub(1) as f32 * self.block_border_spacing
            + insets
            - if nonempty_sections == 0 {
                2.0 * self.block_border_spacing
            } else {
                0.0
            };
        let used = minimum.max(target);
        rows::distribute_table(
            &mut rows,
            &mut self.sections,
            (used - insets).max(0.0),
            self.block_border_spacing,
        );
        for cell in &mut self.cells {
            let layout = cell.block_layout.as_mut().unwrap();
            layout.size = rows[cell.row..cell.row + cell.row_span]
                .iter()
                .map(|row| row.size)
                .sum::<f32>()
                + cell.row_span.saturating_sub(1) as f32 * self.block_border_spacing;
            layout.definite |= !preferred.is_auto() && layout.size > layout.natural_size;
        }
        let mut tracks = Vec::new();
        // Empty sections take up height, but do not introduce cell spacing.
        // With such sections, explicit spacer tracks express the gaps before
        // real rows and after the table that a uniform Grid gap cannot model.
        let explicit_spacing = (self.rows.is_empty()
            || self.sections.iter().any(|section| section.rows.is_empty()))
            && self.block_border_spacing > 0.0;
        if explicit_spacing {
            self.style.gap.height = style_helpers::length(0.0);
            self.style.padding.top =
                style_helpers::length((padding.top - self.block_border_spacing).max(0.0));
            self.style.padding.bottom =
                style_helpers::length((padding.bottom - self.block_border_spacing).max(0.0));
        }
        for section in &self.sections {
            let mut start = tracks.len();
            if section.rows.is_empty() {
                // A section can have height even without a DOM row. Reserve a
                // numeric track without creating an anonymous CSS row box.
                tracks.push(section.size);
            } else {
                for index in section.rows.clone() {
                    if explicit_spacing {
                        tracks.push(self.block_border_spacing);
                        if index == section.rows.start {
                            start += 1;
                        }
                    }
                    self.rows[index].grid_index = tracks.len();
                    tracks.push(rows[index].size);
                }
            }
            self.section_tracks.push(start..tracks.len());
        }
        if explicit_spacing && !self.rows.is_empty() {
            tracks.push(self.block_border_spacing);
        }
        for cell in &mut self.cells {
            cell.style.grid_row.start = style_helpers::line(
                (self.rows[cell.row].grid_index + 1).min(i16::MAX as usize) as i16,
            );
            cell.style.grid_row.end = style_helpers::span(
                (self.rows[cell.row + cell.row_span - 1].grid_index
                    - self.rows[cell.row].grid_index
                    + 1)
                .min(u16::MAX as usize) as u16,
            );
        }
        if tracks.is_empty() {
            tracks.push(
                (used - insets
                    + if explicit_spacing {
                        2.0 * self.block_border_spacing
                    } else {
                        0.0
                    })
                .max(0.0),
            );
        }
        self.style.grid_template_rows = tracks
            .into_iter()
            .map(|size| {
                let track: taffy::TrackSizingFunction = style_helpers::length(size);
                track.into()
            })
            .collect();
        self.style.align_content = Some(taffy::AlignContent::START);
        set_block(
            mode,
            &mut self.style.size,
            style_helpers::length(if self.style.box_sizing == taffy::BoxSizing::ContentBox {
                (used - insets
                    + if explicit_spacing {
                        2.0 * self.block_border_spacing
                    } else {
                        0.0
                    })
                .max(0.0)
            } else {
                used
            }),
        );
        set_block(mode, &mut self.style.min_size, Dimension::auto());
        set_block(mode, &mut self.style.max_size, Dimension::auto());
        space.known_size.block_size = Some(used);
        // Used geometry alone is not a new percentage-resolution guarantee.
        space.into_layout_input()
    }
}

pub(super) fn layout_cell<N>(
    world: &mut LayoutWorld<N>,
    cell: LayoutBoxId,
    mut inputs: LayoutInput,
    mode: WritingMode,
    layout: Option<CellBlockLayout>,
) -> LayoutOutput
where
    N: Copy + Debug + Eq + Hash,
{
    let Some(layout) = layout else {
        return world.compute_child_layout(cell.to_taffy(), inputs);
    };
    set_block(mode, &mut inputs.parent_size, None);
    set_block(
        mode,
        &mut inputs.known_dimensions,
        layout.definite.then_some(layout.size),
    );
    set_block(
        mode,
        &mut inputs.definite_dimensions,
        layout.definite.then_some(layout.size),
    );
    if !layout.definite {
        set_block(
            mode,
            &mut inputs.available_space,
            AvailableSpace::MaxContent,
        );
    }
    let mut output = world.compute_child_layout(cell.to_taffy(), inputs);
    let free = (layout.size - mode.to_logical(output.size).block_size).max(0.0);
    let alignment = world.boxes[cell.index()].style.taffy.align_content;
    let offset = if layout.baseline.is_some() {
        0.0
    } else {
        crate::taffy_tree::single_subject_block_alignment_offset(alignment, free)
    };
    if inputs.run_mode == RunMode::PerformLayout && offset != 0.0 {
        shift_cell_contents(world, cell, offset);
    }
    if let Some(baseline) = &mut output.first_baselines.y {
        *baseline += offset;
    }
    if let Some(baseline) = &mut output.last_baselines.y {
        *baseline += offset;
    }
    if layout.baseline.is_some() && output.first_baselines.y.is_none() {
        // Cells with no line baseline use the content's block-end edge. This
        // must be sampled again after percentage descendants have laid out.
        let padding = world.boxes[cell.index()]
            .style
            .taffy
            .padding
            .resolve_or_zero(inputs.parent_size.width, resolve_stylo_calc_value);
        output.first_baselines.y = Some(if inputs.run_mode == RunMode::PerformLayout {
            (output.content_size.height - padding.bottom).max(0.0)
        } else {
            layout.baseline.unwrap_or(0.0)
        });
    }
    output.content_size.height += offset;
    set_block(mode, &mut output.size, layout.size);
    output
}

/// Blink's restricted-cell first pass sizes direct scrollable percentage
/// children to their initial minimum. The normal second pass restores the
/// authored style and resolves the percentage against the final cell height.
fn restrict_scrollable_percentage_children<N>(
    world: &mut LayoutWorld<N>,
    cell: LayoutBoxId,
    mode: WritingMode,
    restricted: bool,
) -> Vec<(LayoutBoxId, Style<Atom>)>
where
    N: Copy + Debug + Eq + Hash,
{
    if !restricted {
        return Vec::new();
    }
    let mut restored = Vec::new();
    for id in world.boxes[cell.index()].layout_children.clone() {
        let child = &world.boxes[id.index()];
        let style = &child.style.taffy;
        if !child.is_replaced()
            && mode
                .to_logical(style.size)
                .block_size
                .may_have_percentage_dependence()
            && matches!(
                child.style.overflow_modes()[usize::from(mode.is_horizontal())],
                crate::style::LayoutOverflowMode::Auto | crate::style::LayoutOverflowMode::Scroll
            )
        {
            let minimum = mode
                .to_logical(style.min_size)
                .block_size
                .maybe_resolve(None, resolve_stylo_calc_value)
                .unwrap_or(0.0);
            restored.push((id, style.clone()));
            set_block(
                mode,
                &mut world.boxes[id.index()].style.taffy.size,
                Dimension::length(minimum),
            );
            world.cache_clear(id.to_taffy());
        }
    }
    restored
}

fn shift_cell_contents<N>(world: &mut LayoutWorld<N>, cell: LayoutBoxId, offset: f32)
where
    N: Copy + Debug + Eq + Hash,
{
    for child in world.boxes[cell.index()].layout_children.clone() {
        if world.boxes[child.index()].style.taffy.position != taffy::Position::Absolute {
            world.boxes[child.index()].unrounded_layout.location.y += offset;
        }
    }
    if let Some(context) = &mut world.boxes[cell.index()].inline_layout {
        for line in &mut context.line_placements {
            line.translate_block_axis(offset);
        }
        for line in &mut context.fragments.lines {
            line.rect.y += offset;
            line.baseline += offset;
            if let crate::inline::InlinePaintBounds::Bounded(rect) = &mut line.paint_bounds {
                rect.y += offset;
            }
        }
        for text in &mut context.fragments.text {
            text.rect.y += offset;
        }
        for fragment in &mut context.fragments.boxes {
            fragment.rect.y += offset;
        }
    }
}
