module [main]

import Src/Widget

main = {
	map = Dict.empty().in
	widget = Widget.make("demo").
	description = Widget.make("demo").describe().
	multiline = Dict.empty()
		.in
	field_chain = Widget.make("demo").label.
	map_chain = Dict.empty()
		.insert((10, 20), "X")
		.insert((11, 20), "A").in
	var $map = Dict.empty()
	for n in 1..=5 {
		$map = $map.insert((n, n * 2), "A")
		$map = $map.in
	}
}
