#!/usr/bin/env ruby
# frozen_string_literal: true

# Compact a bench/aead/codec results.json into the committed samples.json: every sample and
# observation is kept, times are rounded to 1 µs (the timers resolve 5 µs), and built-site
# artifact entries are kept as recorded. Usage: compact.rb results.json samples.json
require "json"

input, output = ARGV
abort "usage: compact.rb results.json samples.json" unless input && output
data = JSON.parse(File.read(input))
r3 = ->(x) { x.round(3) }
sample = ->(s) { s.is_a?(Numeric) ? r3.(s) : [r3.(s["ms"]), s["encoded"], s["decoded"], r3.(s["cryptoMs"] || 0)] }
observation = lambda do |o|
  {
    "name" => o["name"], "workerMs" => r3.(o["workerMs"]), "encoded" => o["encoded"],
    "decoded" => o["decoded"], "extra" => o["extra"], "mainElapsedMs" => r3.(o["mainElapsedMs"]),
    "pingsSent" => o["pingsSent"], "pingsLost" => o["pingsLost"],
    "pingsAdmittedDuringOp" => o["pingsAdmittedDuringOp"],
    "pingRttMs" => o["pingRttMs"].map(&r3), "pingAnsweredAtMs" => o["pingAnsweredAtMs"].map(&r3),
    "frameGapsMs" => o["frameGapsMs"].map(&r3)
  }.compact
end
out = {
  "format" => "latency samples are [ms, pagesEncoded, pagesDecoded, cryptoMs]; largeInsertMs is ms. " \
              "Times rounded to 1 µs from 5 µs-resolution timers. See bench/aead/codec/README.md.",
  "metadata" => data["metadata"],
  "runs" => data["runs"].map do |run|
    {
      "runIndex" => run["runIndex"], "variantOrder" => run["variantOrder"], "config" => run["config"],
      "pingIntervalMs" => run["pingIntervalMs"], "environment" => run["environment"],
      "idlePingRttMs" => run.dig("idle", "pingRttMs").map(&r3),
      "webCryptoProbe" => (run["webCryptoProbe"] || []).map(&observation),
      "results" => run["results"].map do |v|
        {
          "variant" => v["variant"], "setup" => v["setup"], "finish" => v["finish"],
          "latency" => v["latency"].transform_values { |xs| xs.map(&sample) },
          "observations" => v["observations"].map(&observation)
        }
      end
    }
  end
}
File.write(output, JSON.generate(out) + "\n")
