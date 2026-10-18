# frozen_string_literal: true

module JedNative
  module JsonRelations
    JSON_RELATIONS = %w[json_array_elements jsonb_array_elements json_array_elements_text jsonb_array_elements_text json_each jsonb_each json_each_text jsonb_each_text json_object_keys jsonb_object_keys json_to_record jsonb_to_record json_to_recordset jsonb_to_recordset json_populate_record jsonb_populate_record json_populate_recordset jsonb_populate_recordset].freeze

    def json_relation_columns(source, arguments = nil)
      name = source[:expr][:name]
      type = name.start_with?('jsonb_') ? 'jsonb' : 'json'
      if name.include?('_to_record')
        raise Error.new('42601', 'record function requires a column definition list') unless source[:definitions]
        columns = source[:definitions]
      elsif name.include?('_populate_record')
        arguments ||= source[:expr][:args].map { |arg| compile(arg, []) }
        columns = @types[arguments[0].type]
        raise Error.new('0A000', 'populate_record requires a named composite') unless columns
      elsif name.end_with?('object_keys')
        columns = [{ name: name, type: 'text' }]
      elsif name.include?('_each')
        columns = [{ name: 'key', type: 'text' }, { name: 'value', type: name.end_with?('_text') ? 'text' : type }]
      else
        columns = [{ name: 'value', type: name.end_with?('_text') ? 'text' : type }]
      end
      if columns.any? { |column| column[:type].end_with?('[]') || @types.key?(column[:type]) }
        raise Error.new('0A000', 'JSON record array and composite fields are not supported')
      end
      raise Error.new('42701', 'duplicate record column') unless columns.map { |column| column[:name] }.uniq.length == columns.length
      raise Error.new('42P10', 'too many table function column aliases') if (source[:columns] || []).length > columns.length
      columns.each_with_index.map { |column, i| column.merge(name: source[:columns]&.[](i) || column[:name], table: source[:alias] || name) }
    end

    def source_relation(source)
      node = source[:expr] || source
      name = node[:name]
      unless JSON_RELATIONS.include?(name)
        raise Error.new('42601', 'column definition list is only allowed for record functions') if source[:definitions] && name != 'unnest'
        return super
      end
      arguments = node[:args].map { |arg| compile(arg, []) }
      columns = json_relation_columns(source, arguments)
      values = arguments.map { |arg| evaluate(arg, []) }
      document = values.last
      document = JsonValue.parse(document, binary: name.start_with?('jsonb_')) unless document.nil? || document.is_a?(JsonValue)
      rows = []
      if document.nil?
        rows << (values[0].is_a?(Array) ? values[0].to_a : Array.new(columns.length)) if name.end_with?('_record')
      elsif name.include?('_record')
        list = if name.end_with?('recordset')
          json_require_container(document, :array)
          document.binary ? document.value.map { |item| JsonValue.new(item) } : document.tree[:children].map { |child| document.subtree(child) }
        else [document]
        end
        list.each do |object|
          json_require_container(object, :object)
          data = object.decoded_value
          rows << columns.each_with_index.map do |column, index|
            key = source[:definitions]&.[](index)&.dig(:name) || column[:name]
            if !data.key?(key) && name.include?('_populate_') && values[0]
              values[0][index]
            else
              json_record_field(data[key], column)
            end
          end
        end
      elsif name.include?('array_elements')
        json_require_container(document, :array)
        document.decoded_value if name.end_with?('_text')
        if document.binary
          rows = document.value.map do |item|
            value = JsonValue.new(item)
            result = name.end_with?('_text') ? value.text : value
            charge(result.bytesize) if name.end_with?('_text') && (item.is_a?(Hash) || item.is_a?(Array)) && !arguments.all? { |arg| constant_expression?(arg) }
            [result]
          end
        else
          rows = document.tree[:children].map { |child| [document.subtree(child, text: name.end_with?('_text'))] }
        end
      else
        json_require_container(document, :object)
        document.decoded_value
        entries = if document.binary
          document.value.keys.sort_by { |key| [key.bytesize, key.b] }.map { |key| [key, JsonValue.new(document.value[key])] }
        else
          document.tree[:children].map { |key, child, start, finish| [JsonParser.new(document.to_s[start...finish]).parse, document.subtree(child)] }
        end
        rows = entries.map do |key, value|
          if name.end_with?('_text') && value.binary && (value.value.is_a?(Hash) || value.value.is_a?(Array)) && !arguments.all? { |arg| constant_expression?(arg) }
            charge(value.text.bytesize)
          end
          name.end_with?('object_keys') ? [key] : [key, name.end_with?('_text') ? value.binary ? value.text : value.subtree(value.tree, text: true) : value]
        end
      end
      charge(rows.length)
      Relation.new(columns: columns, rows: rows)
    end

    def json_require_container(document, kind)
      value = document.value
      correct = kind == :object ? value.is_a?(Hash) : value.is_a?(Array)
      return if correct
      document.decoded_value unless value.is_a?(Array) || value.is_a?(Hash)
      raise Error.new('22023', "JSON function requires an #{kind}")
    end

    def json_record_field(item, column)
      return nil if item.nil?
      type = column[:type]
      return json_result_value(item, type) if %w[json jsonb].include?(type)
      value = item.is_a?(String) ? item : JsonValue.render(item)
      result = cast_value(value, type)
      modifier = column[:type_mod]
      result = Types.cast(result, "decimal(#{modifier[:precision]},#{modifier[:scale] || 0})") if modifier && type == 'decimal'
      result
    end
  end
  Engine.prepend(JsonRelations)
end
