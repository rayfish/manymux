# JNA looks up native methods, structures and callbacks by reflection.
-keep class com.sun.jna.** { *; }
-keep class * extends com.sun.jna.Structure { *; }
-keep interface * extends com.sun.jna.Library { *; }
-keep interface * extends com.sun.jna.Callback { *; }
-keep class * implements com.sun.jna.Callback { *; }
-keepattributes RuntimeVisibleAnnotations,AnnotationDefault
-dontwarn java.awt.**
