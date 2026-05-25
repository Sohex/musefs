# beetsplug is a namespace package shared by all beets plugins.
from pkgutil import extend_path

__path__ = extend_path(__path__, __name__)
