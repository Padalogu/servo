/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

// https://svgwg.org/svg2-draft/coords.html#InterfaceSVGTransform
[Exposed=Window]
interface SVGTransform {
  // Transform Types
  const unsigned short SVG_TRANSFORM_UNKNOWN = 0;
  const unsigned short SVG_TRANSFORM_MATRIX = 1;
  const unsigned short SVG_TRANSFORM_TRANSLATE = 2;
  const unsigned short SVG_TRANSFORM_SCALE = 3;
  const unsigned short SVG_TRANSFORM_ROTATE = 4;
  const unsigned short SVG_TRANSFORM_SKEWX = 5;
  const unsigned short SVG_TRANSFORM_SKEWY = 6;

  readonly attribute unsigned short type;
  [SameObject] readonly attribute DOMMatrix matrix;
  readonly attribute unrestricted float angle;

  [Throws] undefined setMatrix(optional DOMMatrix2DInit matrix = {});
  undefined setTranslate(float tx, float ty);
  undefined setScale(float sx, float sy);
  undefined setRotate(float angle, float cx, float cy);
  undefined setSkewX(float angle);
  undefined setSkewY(float angle);
};
