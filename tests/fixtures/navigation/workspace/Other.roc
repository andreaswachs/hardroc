import Color

Other :: {}.{
	describe : Color -> Str
	describe = |color| Color.to_str(color)
}
