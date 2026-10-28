# frozen_string_literal: true

module JedNative
  # The handle's scratch_encryption setting (spec/design/encryption.md §6; encrypted.md §11): fixed
  # at open, `auto` (sealed when the main database is encrypted) or `on`.
  module ScratchEncryptionSetting
    def initialize(scratch_encryption: :auto, **options)
      setting = scratch_encryption.to_s.downcase.to_sym
      raise Error.new('22023', 'scratch_encryption must be auto or on') unless %i[auto on].include?(setting)
      @scratch_encryption = setting
      super(**options)
    end

    attr_reader :scratch_encryption

    def scratch_sealed? = @scratch_encryption == :on || !!@storage&.encrypted?

    # The scratch stream: a key-entropy stream of the handle's own, so scratch keys never perturb a
    # database's stream (encrypted.md §11).
    def scratch_stream
      @scratch_stream ||= (@environment ||= Environment.new).key_stream
    end
  end
  Engine.prepend(ScratchEncryptionSetting)
end
