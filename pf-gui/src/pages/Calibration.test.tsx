import { render, screen, waitFor } from '@testing-library/react'
import { beforeEach, describe, expect, it, vi } from 'vitest'

const h = vi.hoisted(() => ({
  listCalibrations: vi.fn(),
}))

vi.mock('../api/bridge', () => {
  class BridgeFailure extends Error {
    code = 'unknown'
    detail?: string
  }
  return {
    BridgeFailure,
    isTauri: () => true,
    bridge: h,
  }
})

import { CalibrationPage } from './Calibration'

describe('CalibrationPage', () => {
  beforeEach(() => {
    vi.resetAllMocks()
    h.listCalibrations.mockResolvedValue([])
  })

  it('renders a deliberate empty state that explains what is calibratable', async () => {
    render(<CalibrationPage onNotice={() => {}} />)
    await waitFor(() => expect(h.listCalibrations).toHaveBeenCalled())
    expect(
      await screen.findByText(/Fit an estimated display-power model/),
    ).toBeTruthy()
    expect(screen.getByText('Create display calibration')).toBeTruthy()
    expect(screen.getByText('What is calibratable')).toBeTruthy()
    expect(screen.getByText('Display power')).toBeTruthy()
    expect(screen.getByText('Calibrated values remain Estimated')).toBeTruthy()
  })
})
