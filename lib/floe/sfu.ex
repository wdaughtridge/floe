defmodule Floe.SFU do
  use Rustler, otp_app: :floe, crate: "floe_sfu"

  def start_link(), do: :erlang.nif_error(:nif_not_loaded)

  def put_new_whep_client(_sdp_offer, _link), do: :erlang.nif_error(:nif_not_loaded)

  def put_new_whip_client(_sdp_offer, _link), do: :erlang.nif_error(:nif_not_loaded)
end
