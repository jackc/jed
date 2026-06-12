# frozen_string_literal: true

# scripts/bench_html.rb — render one benchmark run as a self-contained static HTML
# report (spec/design/benchmarks.md §9): per-benchmark bar charts sorted fastest-first,
# relative multipliers, and — when a baseline run is available — before/after deltas.
# Stdlib only (JSON + ERB), inline CSS, zero JavaScript; the page opens in any browser
# or the VS Code preview. The report model is shared with bench_markdown.rb
# (BenchResults.report_model).
#
#   ruby scripts/bench_html.rb [run_dir] [baseline_dir] [--no-baseline]
#
# `run_dir` defaults to the newest dir under bench/results/; `baseline_dir` defaults to
# the run immediately preceding it (--no-baseline suppresses the comparison). Writes
# <run_dir>/report.html and prints its path. Applies the same verification as
# bench_report.rb — exits 1 on any checksum mismatch or mixed fingerprints (in the run
# or the baseline) — but still writes the page, with the failures in a red banner, so
# the failure is viewable. A baseline whose data fingerprint differs from the run's is
# a warning (deltas may not be comparable), not a failure.

require_relative "bench_results"
require "erb"

def h(text)
  ERB::Util.html_escape(text)
end

no_baseline = ARGV.delete("--no-baseline")
run_dir, baseline_dir = BenchResults.resolve_dirs(ARGV[0], ARGV[1], baseline: !no_baseline)

results = BenchResults.load_dir(run_dir)
baseline = baseline_dir && BenchResults.load_dir(baseline_dir)
failures = BenchResults.verification_failures(results, baseline)

run_fp = results.first["fingerprint"]
baseline_fp = baseline&.first&.fetch("fingerprint")
fingerprint_warning = baseline && baseline_fp != run_fp

joined = baseline ? BenchResults.join_runs(results, baseline) : []
model = BenchResults.report_model(results, joined)
sections = model[:sections]
baseline_only_benches = model[:baseline_only_benches]

template = <<~'HTML'
  <!doctype html>
  <html lang="en">
  <head>
  <meta charset="utf-8">
  <title>jed bench — <%= h(File.basename(run_dir)) %></title>
  <style>
    body { font: 14px/1.5 system-ui, sans-serif; color: #1f2937; background: #fafaf9;
           max-width: 980px; margin: 2rem auto; padding: 0 1rem; }
    h1 { font-size: 1.4rem; margin-bottom: .1rem; }
    .meta { color: #6b7280; font-size: .85rem; margin: .1rem 0; }
    .banner { padding: .5rem .75rem; border-radius: 6px; margin: 1rem 0; font-size: .9rem; }
    .banner.ok { background: #ecfdf5; color: #065f46; border: 1px solid #a7f3d0; }
    .banner.warn { background: #fffbeb; color: #92400e; border: 1px solid #fde68a; }
    .banner.fail { background: #fef2f2; color: #991b1b; border: 1px solid #fecaca; white-space: pre-wrap; }
    section { margin: 1.75rem 0; }
    h2 { font-size: 1.05rem; margin: 0 0 .1rem; }
    h2 .dataset { color: #6b7280; font-weight: normal; font-size: .85rem; }
    .desc { margin: .1rem 0; color: #4b5563; font-size: .9rem; }
    .sql { margin: .1rem 0 .5rem; }
    code { background: #f3f4f6; padding: .1rem .3rem; border-radius: 4px; font-size: .8rem; }
    .subtitle { color: #9ca3af; font-size: .75rem; margin: .1rem 0 .4rem; }
    table { border-collapse: collapse; width: 100%; }
    td, th { padding: .25rem .5rem; font-size: .85rem; }
    th { text-align: left; color: #6b7280; font-weight: 500; border-bottom: 1px solid #e5e7eb; }
    th.num, td.num { text-align: right; font-variant-numeric: tabular-nums; }
    td.label { white-space: nowrap; width: 1%; }
    tr.jed td.label { font-weight: 600; color: #b45309; }
    td.barcell { width: 45%; }
    .bar { height: 12px; border-radius: 3px; min-width: 3px; }
    .bar.jed { background: #f59e0b; }
    .bar.postgres { background: #3b82f6; }
    .bar.sqlite { background: #9ca3af; }
    td.delta.imp { color: #15803d; }
    td.delta.reg { color: #b91c1c; }
    td.delta.noise { color: #9ca3af; }
    .note { color: #6b7280; font-size: .8rem; margin: .3rem 0; }
    footer { margin-top: 2.5rem; color: #9ca3af; font-size: .8rem;
             border-top: 1px solid #e5e7eb; padding-top: .75rem; }
  </style>
  </head>
  <body>
  <h1>jed benchmarks — <%= h(File.basename(run_dir)) %></h1>
  <p class="meta">
    <% if baseline_dir -%>baseline: <%= h(File.basename(baseline_dir)) %> ·<% end -%>
    fingerprint <%= h(run_fp[0, 12]) %> · <%= results.size %> results ·
    generated <%= Time.now.utc.strftime("%Y-%m-%dT%H:%M:%SZ") %>
  </p>
  <% if failures.empty? -%>
  <div class="banner ok">all <%= results.size %> results agree on answer checksums ✓</div>
  <% else -%>
  <div class="banner fail">FAIL:
  <%= failures.map { |f| h(f) }.join("\n") %></div>
  <% end -%>
  <% if fingerprint_warning -%>
  <div class="banner warn">baseline has a different data fingerprint
    (<%= h(baseline_fp[0, 12]) %> vs <%= h(run_fp[0, 12]) %>) — the runs measured
    different data; deltas may not be comparable</div>
  <% end -%>
  <% sections.each do |s| -%>
  <section>
  <h2><%= h(s[:bench]) %> <span class="dataset"><%= h(s[:dataset]) %></span></h2>
  <% if s[:description] -%><p class="desc"><%= h(s[:description]) %></p><% end -%>
  <% if s[:sql] -%><p class="sql"><code><%= h(s[:sql]) %></code></p><% end -%>
  <p class="subtitle"><%= h(s[:subtitle]) %></p>
  <table>
  <tr><th></th><th></th><th class="num">ns/op</th><th class="num">vs fastest</th>
  <% if baseline_dir -%><th class="num">Δ vs baseline</th><% end -%></tr>
  <% s[:rows].each do |r| -%>
  <tr class="<%= r[:family] %>">
    <td class="label"><%= h(r[:label]) %></td>
    <td class="barcell"><div class="bar <%= r[:family] %>" style="width: <%= format("%.1f", r[:width_pct]) %>%"></div></td>
    <td class="num" title="<%= h(r[:tooltip]) %>"><%= h(r[:ns]) %></td>
    <td class="num"><%= h(r[:mult]) %></td>
  <% if baseline_dir -%>
    <% if r[:delta] -%>
    <td class="num delta <%= BenchResults.delta_class(r[:delta]) %>" title="<%= h(r[:delta_tooltip]) %>"><%= format("%+.1f%%", r[:delta]) %></td>
    <% else -%>
    <td class="num delta noise">—</td>
    <% end -%>
  <% end -%>
  </tr>
  <% end -%>
  </table>
  <% unless s[:missing].empty? -%>
  <p class="note">in baseline but not in this run: <%= h(s[:missing].join(", ")) %></p>
  <% end -%>
  </section>
  <% end -%>
  <% unless baseline_only_benches.empty? -%>
  <p class="note">benches in the baseline but not in this run:
    <%= h(baseline_only_benches.map { |b, d| "#{b} (#{d})" }.join(", ")) %></p>
  <% end -%>
  <footer>
    lower is better; bars are linear within each benchmark; Δ gray under <%= BenchResults::NOISE_PCT.to_i %>%
    (single-run wall-clock noise — spec/design/benchmarks.md §10).
    Regenerate: <code>rake bench:html</code>.
  </footer>
  </body>
  </html>
HTML

out_path = File.join(run_dir, "report.html")
File.write(out_path, ERB.new(template, trim_mode: "-").result(binding))
puts out_path

unless failures.empty?
  failures.each { |f| warn "FAIL: #{f}" }
  exit 1
end
