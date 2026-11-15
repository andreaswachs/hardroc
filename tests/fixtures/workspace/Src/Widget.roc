import ../Color

# A labelled widget. The module imports `Color` and renders its tint with it.
Widget := {
	label : Str,
}.{

	## Build a widget with the default color.
	make : Str -> Widget
	make = |label| Widget.({ label: label })

	## Describe the widget and the tint it would use.
	describe : Widget -> Str
	describe = |widget| "${widget.label}: ${Color.base.to_hex()}"
}
