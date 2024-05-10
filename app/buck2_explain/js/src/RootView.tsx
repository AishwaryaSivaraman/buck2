/**
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This source code is licensed under both the MIT license found in the
 * LICENSE-MIT file in the root directory of this source tree and the Apache
 * License, Version 2.0 found in the LICENSE-APACHE file in the root directory
 * of this source tree.
 */

import React, {useContext} from 'react'
import {Target} from './Target'
import {DataContext} from './App'
import {RootSpan} from './RootSpan'

/**
 * Shows the root target
 */
export function RootView(props: {view: string}) {
  const {rootTarget} = useContext(DataContext)

  let targetElement
  if (rootTarget == null) {
    targetElement = <p>No root target</p>
  } else {
    targetElement = <Target target={rootTarget} />
  }

  return (
    <>
      {rootTarget ? <RootSpan /> : null}
      {targetElement}
    </>
  )
}
