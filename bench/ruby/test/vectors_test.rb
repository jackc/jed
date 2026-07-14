# frozen_string_literal: true

# Pins the splitmix64 + FNV-1a ports to the SAME vectors as bench/rust/src/lib.rs and
# bench/ts/tests/prng.test.ts (benchmarks.md §4/§6). These are the cross-language agreement
# contract: if the param stream or the answer checksum drifts, the gem bench's cross-engine
# checksum would silently disagree with the core's. Run: `mise exec -- ruby bench/ruby/test/vectors_test.rb`.

require "minitest/autorun"
$LOAD_PATH.unshift(File.expand_path("../lib", __dir__))
require "bench"

class VectorsTest < Minitest::Test
  def test_prng_vectors
    {
      1 => [0x910a2dec89025cc1, 0xbeeb8da1658eec67, 0xf893a2eefb32555e,
            0x71c18690ee42c90b, 0x71bb54d8d101b5b9],
      1_234_567 => [0x599ed017fb08fc85, 0x2c73f08458540fa5, 0x883ebce5a3f27c77,
                    0x3fbef740e9177b3f, 0xe3b8346708cb5ecd],
    }.each do |seed, want|
      prng = Bench::Prng.new(seed)
      want.each_with_index do |w, i|
        assert_equal w, prng.next_u64, "seed #{seed} output #{i}"
      end
    end
  end

  def test_checksum_vector
    c = Bench::Checksum.new
    c.int(1)
    c.null
    c.text("abc")
    c.end_row
    c.int(-7)
    c.end_row
    assert_equal "dd6e60407d30d28b", c.hex
  end

  def test_lower_sample_percentiles
    samples = (0..10).to_a
    assert_equal 5, Bench.percentile(samples, 50)
    assert_equal 9, Bench.percentile(samples, 90)
    assert_equal 9, Bench.percentile(samples, 99)
  end
end
