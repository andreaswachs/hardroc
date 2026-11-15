module [top_pair, make]

Pair(a, b) : { first : a, second : b }

top_pair : Pair(U64, Str)
top_pair = {
	first: 1,
	second: "x",
}

make = |_| {
	pair : Pair(U64, Str)
	pair = {
		first: 1,
		second: "x",
	}
	pair
}
