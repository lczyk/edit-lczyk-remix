# Comments
# Single-line comment

=begin
Multi-line
comment
=end

# Numbers
42
3.14
0.5
1e10
1.5e-3
0xff
0xFF
0b1010
0o77
1_000_000
3.14r
2i

# Constants
true
false
nil
Object
String

# Strings
'single quotes with escape: \' \n \t \\'
"double quotes with escape: \" \n \t \\"
`echo shell command`

# Symbols and variables
:symbol
:method_name?
@instance_var
@@class_var
$global_var

# Control flow keywords
if true
  puts "yes"
elsif false
  puts "no"
else
  puts "maybe"
end

unless nil
  puts "not nil"
end

case 42
when 1
  puts "one"
else
  puts "other"
end

for i in 1..3
  next if i == 2
  break if i == 3
end

while false
  redo
end

begin
  raise "oops"
rescue StandardError => e
  retry
ensure
  puts e
end

# Definitions and method calls
module Demo
  class Animal
    def initialize(name)
      @name = name
    end

    def speak! =  puts "#{@name} speaks"
  end
end

alias old_speak speak!
undef old_speak

BEGIN { puts "start" }
END { puts "finish" }
defined? Demo
self
super
yield

puts "hello"
Array.new(3)
greet("world")

# Singleton method defs
class Foo
  def self.bar
    42
  end

  def Foo.baz
    99
  end
end

# Percent literals
words = %w[one two three]
syms = %i[a b c]
str_q = %q(plain)
str_Q = %Q{interp}

# Character literals
ch_a = ?a
ch_z = ?Z
ch_esc = ?\n

# Strings carry raw newlines, single- and double-quoted alike.
raw = 'line one
line two'
esc = "escaped \" quote
line two"

# Heredocs: squiggly, dashed, quoted, and plain. The rest of the opener line
# is still code; the body starts on the next line.
sql = <<~SQL.strip
  SELECT * FROM users
  WHERE name = 'SQL'
SQL
html = <<-'HTML'
    <p>#{not_interpolated}</p>
    HTML
plain = <<EOS
EOS mid-line stays body
EOS
puts sql
greeting = <<~MSG
  hi #{name}, \t#{items.map { |i| i * 2 }.sum} total
MSG

# Interpolation is code: its quotes and braces don't end the string.
say = "stop=#{final["reason"] || "?"} n=#{h.fetch(:k) { {} }.size}"
multi = "a #{
  value
} b"
tick = `ls #{dir}`

# Regexp literals vs division
x = /\d+ #{n} \/ \w/i
line =~ /^#/ && line !~ /x/
assert_match(/nope` not found/, msg)
words.grep /re/
when /a|b/
half = total / 2
ratio = a/b
total /= 2

# Multi-line and nested percent literals
list = %w[one two
          three]
rx = %r{\d{3}-\d{4}}

# Labels, symbols, scope
opts = { in: 1, class: 2, if: 3 }
call(key: v, other: 1)
def kw(a:, b: 1) end
pick = flag ? left : right
Foo::Bar
sum = xs.reduce(:+)
sorted = xs.sort_by(&:size)

# Method names after a dot are never keywords
x.class
x.nil?
x.then { _1 }

# Underscores and globals
a, _, c = triple
$stderr.puts $0, $!

# Block params
xs.each { |k, v| p k }
xs.each_with_index do |item, i|
end

require "json"
require_relative "lib"
include Comparable
attr_reader :name
private

__END__
anything goes here: "unclosed `string
