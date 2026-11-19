//! Figure block of the deep research instructions (docs/specs/harness/deep-research.md, "Figures").

pub(crate) const INSTRUCTIONS: &str = r#"<deep-research-figures>
Every Deep Research paper carries figures, not only prose. Plan, style, draw and review each figure before it goes into report.tex, using only tools already on this machine.

WHEN A FIGURE EARNS ITS PLACE
- Add a figure only when it carries information the sentences cannot: a pipeline or architecture diagram, a taxonomy of the approaches you compare, a comparison across the sources you read, a trend over time, a distribution, a cost or scale table turned into a plot.
- Two to six figures for a full report. No decorations, no stock icons, no figure that repeats a table.
- Every figure must be referenced from the body text as Figure~\ref{fig:...} and must be explained in one or two sentences. A figure nobody mentions is deleted.

WHERE FIGURES LIVE
- Put every figure in research/<short-topic-slug>/figures/.
- Keep the generator next to its output: fig-<name>.tex (TikZ/PGFPlots), fig-<name>.dot (Graphviz), fig-<name>.py (matplotlib), plus the rendered fig-<name>.pdf or .svg.
- report.tex embeds them with \input{figures/fig-<name>} for TikZ, or with \includegraphics[width=\linewidth]{figures/fig-<name>} for a rendered file. Keep figures/ next to report.tex so the document still compiles when the folder is copied.

ALLOWED RENDERERS, IN THIS ORDER
1. TikZ and PGFPlots written directly in a .tex file. This is the default: it needs nothing but the LaTeX engine that builds report.tex.
2. Graphviz dot, drawing a diagram, only when dot is already on PATH (check first: Get-Command dot on Windows, command -v dot elsewhere).
3. Python matplotlib, drawing a data plot, only when python or python3 is already on PATH and import matplotlib succeeds.
- Check availability before you rely on a tool, once per run.
- Never install, upgrade, download or package-manage anything: no pip install, no conda, no winget, choco, brew or apt, no font or model downloads. Harness does not install figure tools.
- When dot and matplotlib are both missing, use PGFPlots for plots and pure TikZ for diagrams. Never fall back to ASCII art or to a description of a figure that was not drawn.
- In report.tex use only graphicx, tikz and pgfplots on top of the packages already listed, and only when the figure needs them. If a drawing would need an exotic package, simplify the drawing instead.

THE LOOP: PLAN, STYLE, DRAW, CRITIQUE
1. Plan. Write one line per candidate figure: the title, the single claim it makes, the type (diagram, bar, line, scatter, heatmap), the data source (the report section, the table, or the sources with their citation keys), the exact numbers and labels to show, and the caption. Drop a candidate whose claim you cannot back with data you actually read.
2. Style. Turn the plan into concrete drawing decisions with the style rules below: palette, fonts, sizes, layout, legend and annotation placement. Keep the plan's content unchanged; this step is aesthetic only.
3. Draw. Write the generator file in figures/, render it, and insert the block:
   \begin{figure}[htbp]
     \centering
     <\input{figures/fig-<name>} or \includegraphics[width=\linewidth]{figures/fig-<name>}>
     \caption{<what is shown and what it shows>}
     \label{fig:<name>}
   \end{figure}
   Captions state the content and the takeaway, cite the sources of the data, and do not start with "Figure 1:" because LaTeX numbers it. Labels use fig:<name>.
4. Critique, once. After rendering, read the output (open the PDF or image, or inspect the source carefully when you cannot view it) and check it against the plan, the body text and the numbers: right values, no invented data, readable labels, no overlap, no clipped text, legend explains what it should, caption matches the drawing. Fix what is wrong and re-render once.
- If a figure is still wrong or misleading after that single pass, delete it and say so in the reply instead of shipping it. One honest missing figure is better than a plausible wrong one.

STYLE RULES FOR FIGURES
- Background: white. No gradients, no drop shadows, no 3D bars or pies, no bevels, no clip art.
- Colour: one muted categorical palette for the whole paper, for example slate #4C72B0, terracotta #C44E52, olive #6B8E23, sand #DD8452, mauve #8172B3, teal #55A868. Use viridis or magma for sequential scales and coolwarm for diverging ones. Never the rainbow or jet scale. Never more than six colours in one figure.
- Accessible and print-safe: differentiate series by colour and by marker shape, hatch or line style, so the figure still reads in greyscale and for colourblind readers. Keep text black on white.
- Typography: sans-serif labels, same size as the body text or one step smaller, nothing below 7 pt at final size. Axis labels name the quantity and its unit. No all-caps labels, no label longer than about thirty characters; wrap or shorten instead.
- Layout: one column wide by default, \linewidth at most; two panels side by side only when they are compared directly. Nothing important in the outer 5 percent of the canvas.
- Plots: light grey dashed gridlines behind the data, tick labels horizontal unless they would overlap, then rotated 45 degrees. Lines carry markers; dashed lines for baselines and reference values; shaded translucent bands for uncertainty rather than bare error bars; black flat caps on bar error bars. Legends go inside the plotting area or in one row above it, direct labels next to lines are better than a legend. Heatmaps use square cells with the value printed in them. Every axis starts where honesty requires, and a truncated axis is labelled as such.
- Diagrams: boxes with rounded corners, one accent colour per role, arrows with short labels on the flow, consistent left-to-right or top-to-bottom direction, at most about fifteen nodes. Group related parts inside a labelled container. Text inside the drawing must be real LaTeX text, not an image.
- No text inside the drawing that duplicates the caption. No legend that only restates the colours named in the caption.

HONESTY
- Every number in a figure comes from a source you opened or from work you did in this run. Never invent data points, axis ranges or percentages.
- When the sources disagree, a figure may show the disagreement, labelled as such; it must not average it away silently.
- Say in the reply which figures were drawn, by which renderer (TikZ, Graphviz or matplotlib), which one was dropped, and where the files are.
Write figure text in the language of the report.
</deep-research-figures>"#;

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    #[test]
    fn figure_instructions_cover_the_local_renderers_only() {
        let text = super::INSTRUCTIONS;
        assert!(text.contains("research/<short-topic-slug>/figures/"));
        assert!(text.contains("pgfplots") && text.contains("dot") && text.contains("matplotlib"));
        assert!(text.contains("\\begin{figure}") && text.contains("\\label{fig:"));
        assert!(text.contains("Never install"));
        assert!(!text.contains("pip install -r"));
    }

    #[test]
    fn figure_instructions_forbid_the_rainbow_scale() {
        assert!(super::INSTRUCTIONS.contains("viridis"));
        assert!(super::INSTRUCTIONS.contains("Never the rainbow or jet scale"));
    }
}
